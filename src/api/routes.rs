use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;

use crate::assets;
use crate::error::{Error, Result};
use crate::model::{ColumnInfo, SourceConfig, SourceSummary, TableInfo};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sources", get(list_sources).post(add_source))
        // Not `/sources/test`: a static segment would shadow a source whose id
        // happens to be `test`.
        .route("/connection-test", post(test_connection))
        .route("/shells", get(super::terminal::shells))
        .route("/sources/{id}", delete(remove_source).put(update_source))
        .route("/sources/{id}/status", get(source_status))
        .route("/sources/{id}/databases", get(databases))
        .route("/sources/{id}/tables", get(tables))
        .route("/sources/{id}/columns", get(columns))
        .route("/sources/{id}/schema", get(schema))
        .route("/search", get(search))
        .route("/files", get(data_files))
        .route("/auth/entra", post(start_sign_in))
        .route("/auth/entra/{ticket}", get(sign_in_status))
        .route(
            "/workspace",
            get(workspace).put(open_workspace).delete(close_workspace),
        )
        .route("/workspace/file", get(read_file).put(write_file))
        .route("/ws/query", get(super::ws::query))
        .route("/ws/terminal", get(super::terminal::terminal))
        .fallback(assets::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "vault": state.vault().describe(),
        // The UI hides the terminal pane when this is false.
        "terminal": state.terminal_enabled,
    }))
}

async fn list_sources(State(state): State<Arc<AppState>>) -> Json<Vec<SourceSummary>> {
    Json(state.summaries().await)
}

/// Register a source. The body carries the credentials; nothing that comes back
/// out of the API ever does.
async fn add_source(
    State(state): State<Arc<AppState>>,
    Json(config): Json<SourceConfig>,
) -> Result<(StatusCode, Json<SourceSummary>)> {
    check(&config)?;
    // The sign-in becomes the credential here, before anything is stored: the
    // record keeps the refresh token, the connection below uses an access token
    // minted from it.
    let config = state.redeem_sign_in(config)?;
    prove(&state, &config).await?;
    let summary = state.register(config).await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

/// Everything the wire format cannot say about itself.
fn check(config: &SourceConfig) -> Result<()> {
    if config.id.trim().is_empty() {
        return Err(Error::BadRequest("source id must not be empty".into()));
    }
    if config.instance.is_some() && config.port.is_some() {
        return Err(Error::BadRequest(
            "set either `port` or `instance`, not both".into(),
        ));
    }
    // `host` is optional in the wire format so a folder source can omit it; for
    // everything else, an absent one would be a connection attempt to nowhere.
    // The mirror of this — a folder source without a path — is caught by its
    // connector, which also covers records loaded from disk.
    if config.kind.needs_host() && config.host.trim().is_empty() {
        return Err(Error::BadRequest("source needs a host".into()));
    }
    // Format options describe files. Accepting them on a server source would
    // record something that can never be read back out.
    if config.kind.is_server() && config.options != crate::model::FileOptions::default() {
        return Err(Error::BadRequest(
            "format options only apply to a folder or file source".into(),
        ));
    }
    Ok(())
}

/// Reject bad credentials now rather than on the first query, and before the
/// secret goes anywhere near the vault.
async fn prove(state: &AppState, config: &SourceConfig) -> Result<()> {
    let connecting = state.authorise(config.clone(), None).await?;
    state.connector(connecting.kind).connect(&connecting).await?;
    Ok(())
}

/// What the dialogue sends when it is editing rather than adding.
#[derive(Deserialize)]
struct SourceUpdate {
    #[serde(flatten)]
    config: SourceConfig,
    /// Use the credential already in the vault instead of the one in `auth`.
    ///
    /// The password is never sent back out of the API, so a form reopened on an
    /// existing source has no way to send it back in. An empty box means *keep
    /// it*; typing in one means change it.
    #[serde(default)]
    keep_secret: bool,
}

/// Replace a source: rename it, point it elsewhere, or give it a new password.
///
/// The same shape as adding one, including connecting before anything is written,
/// so an edit that would break the source fails in the dialogue rather than at the
/// next query.
async fn update_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(update): Json<SourceUpdate>,
) -> Result<Json<SourceSummary>> {
    check(&update.config)?;
    let config = if update.keep_secret {
        state.keep_secret(&id, update.config).await?
    } else {
        state.redeem_sign_in(update.config)?
    };
    prove(&state, &config).await?;
    Ok(Json(state.replace(&id, config).await?))
}

// ---------------------------------------------------------------- Entra sign-in

#[derive(Deserialize)]
struct SignInRequest {
    /// Which source kind the token is for. Entra mints per-resource tokens, so
    /// signing in "to Azure" is not a thing that exists.
    kind: crate::model::SourceKind,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    /// `true` when the browser is not on the machine running alkyon.
    #[serde(default)]
    device_code: bool,
}

/// Begin a sign-in and hand back the ticket that stands for it.
///
/// Returns immediately in both flows: the browser one because the browser has
/// only just opened, the device one because there is a code to show first. What
/// happens next is watched through `GET /auth/entra/{ticket}`.
async fn start_sign_in(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SignInRequest>,
) -> Result<Json<Value>> {
    let resource = body.kind.entra_resource().ok_or_else(|| {
        Error::BadRequest("this kind of source is not reached through Entra".into())
    })?;
    let credential = crate::azure::entra::Credential {
        tenant: body
            .tenant
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| crate::azure::entra::DEFAULT_TENANT.to_owned()),
        client_id: body
            .client_id
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| crate::azure::entra::AZURE_CLI_CLIENT_ID.to_owned()),
    }
    .validated()?;

    if body.device_code {
        let (ticket, code) = state.sign_ins.start_device(credential, resource).await?;
        return Ok(Json(json!({
            "ticket": ticket,
            "user_code": code.user_code,
            "verification_uri": code.verification_uri,
            "expires_in": code.expires_in,
        })));
    }

    let ticket = state.sign_ins.start_interactive(credential, resource);
    Ok(Json(json!({ "ticket": ticket })))
}

/// How a sign-in is getting on. Polled by the dialogue while it waits.
async fn sign_in_status(
    State(state): State<Arc<AppState>>,
    Path(ticket): Path<String>,
) -> Json<Value> {
    let status = state.sign_ins.status(&ticket);
    let mut body = serde_json::to_value(&status).unwrap_or_else(|_| json!({ "status": "unknown" }));
    // Re-offer the code on every poll, so a page reloaded mid-sign-in can still
    // tell the user what to type.
    if let (Some(prompt), Some(object)) = (state.sign_ins.prompt(&ticket), body.as_object_mut()) {
        object.insert("user_code".into(), json!(prompt.user_code));
        object.insert("verification_uri".into(), json!(prompt.verification_uri));
    }
    Json(body)
}

/// What the dialogue's *Test* button sends.
#[derive(Deserialize)]
struct TestRequest {
    #[serde(flatten)]
    config: SourceConfig,
    /// The source whose stored credential to use, when testing an **edit**.
    ///
    /// Without this, *Test* would contradict the sentence next to the password
    /// box: the form says an empty box keeps the stored password, and then testing
    /// would send the empty one and be told the login failed.
    #[serde(default)]
    keep_secret_of: Option<String>,
}

/// Try the credentials without registering anything — the dialogue's *Test*
/// button. `POST /sources` already validates before saving; this is the same
/// check without the commitment.
async fn test_connection(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TestRequest>,
) -> Result<Json<Value>> {
    let started = Instant::now();
    let config = match &request.keep_secret_of {
        Some(key) => state.keep_secret(key, request.config).await?,
        None => state.borrow_sign_in(request.config)?,
    };
    // Nothing is stored, so nothing is cached and no rotated token is written
    // back: this token is minted for one connection and dropped.
    let config = state.authorise(config, None).await?;
    state.connector(config.kind).connect(&config).await?;
    Ok(Json(json!({
        "ok": true,
        "database": config.database(),
        "latency_ms": started.elapsed().as_millis() as u64,
    })))
}

/// Is this source reachable right now? Always 200 — the answer is in the body,
/// because "the server is down" is a normal state for the UI to render, not a
/// request that failed.
async fn source_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    // A missing source is a genuine 404; a refused connection is not.
    state.record(&id).await?;

    let started = Instant::now();
    Ok(Json(match state.open(&id, None).await {
        Ok(_) => json!({ "ok": true, "latency_ms": started.elapsed().as_millis() as u64 }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }))
}

async fn remove_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state.remove(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn databases(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<String>>> {
    let connection = state.open(&id, None).await?;
    Ok(Json(connection.list_databases().await?))
}

// --------------------------------------------------------- schema and search

#[derive(Deserialize)]
struct SchemaQuery {
    db: Option<String>,
    /// Reload even if a snapshot is already cached.
    #[serde(default)]
    refresh: bool,
}

/// The whole schema of one database: every table with its columns.
///
/// Fetched when a source becomes active, which is what lets autocompletion offer
/// columns before you have browsed to them, and what fills the search index.
async fn schema(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<SchemaQuery>,
) -> Result<Json<Value>> {
    let snapshot = state
        .snapshot(&id, params.db.as_deref(), params.refresh)
        .await?;
    Ok(Json(json!({
        "source": snapshot.source,
        "database": snapshot.database,
        "tables": snapshot.tables,
        "columns": snapshot.column_count(),
    })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

/// Search tables, columns and column types across every cached schema.
///
/// Only what is indexed is searched — `indexed` says what that covers, so the UI
/// can offer to index the rest rather than quietly under-reporting.
async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<Value>> {
    let cache = state.schema();
    let hits = cache.search(&params.q, params.limit.unwrap_or(50)).await;
    let indexed: Vec<Value> = cache
        .indexed()
        .await
        .into_iter()
        .map(|(source, database, columns)| {
            json!({
                "source": source,
                "database": database,
                "columns": columns,
            })
        })
        .collect();

    Ok(Json(json!({ "hits": hits, "indexed": indexed })))
}

#[derive(Deserialize)]
struct DataFilesQuery {
    /// A folder on the machine running alkyon, `~` allowed.
    path: String,
    /// Which file type the source will read. Absent lists every readable file.
    format: Option<crate::model::FileFormat>,
}

/// The files a folder source at `path` would read, relative to it.
///
/// The source dialog's file list: registering a folder is where you say "just this
/// one file, actually", and nothing else can tell you what there is to choose from.
async fn data_files(Query(params): Query<DataFilesQuery>) -> Result<Json<Value>> {
    let root = crate::workspace::resolve_root(&params.path)?;
    if !root.is_dir() {
        return Err(Error::BadRequest(format!(
            "`{}` is not a folder",
            params.path
        )));
    }
    let options = crate::model::FileOptions {
        format: params.format,
        ..Default::default()
    };
    Ok(Json(json!({
        "files": crate::connectors::files::candidates(&root, &options),
    })))
}

// ------------------------------------------------------------------ workspace

/// The open folder and the files under it alkyon can do something with — `.sql`
/// to edit, data files to register as a source. `root: null` when nothing is open.
async fn workspace(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let recent = recent(&state).await;
    let Some(root) = state.workspace().await else {
        return Ok(Json(
            json!({ "root": null, "files": [], "truncated": false, "recent": recent }),
        ));
    };
    let (files, truncated) = crate::workspace::list(&root)?;
    Ok(Json(json!({
        "root": root.to_string_lossy(),
        "files": files,
        "truncated": truncated,
        "recent": recent,
    })))
}

/// Folders opened before, for the start screen to offer.
async fn recent(state: &AppState) -> Vec<String> {
    state
        .recent_folders()
        .await
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

#[derive(Deserialize)]
struct OpenWorkspace {
    /// A path on the machine running alkyon, `~` allowed.
    path: String,
}

async fn open_workspace(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OpenWorkspace>,
) -> Result<Json<Value>> {
    let root = state.open_workspace(&body.path).await?;
    let (files, truncated) = crate::workspace::list(&root)?;
    Ok(Json(json!({
        "root": root.to_string_lossy(),
        "files": files,
        "truncated": truncated,
        "recent": recent(&state).await,
    })))
}

async fn close_workspace(State(state): State<Arc<AppState>>) -> Result<StatusCode> {
    state.close_workspace().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct FileQuery {
    /// Relative to the workspace root.
    path: String,
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileQuery>,
) -> Result<Json<Value>> {
    let root = state.workspace_root().await?;
    let file = crate::workspace::resolve(&root, &params.path, true)?;
    let text = crate::workspace::strip_bom(tokio::fs::read_to_string(&file).await?);
    Ok(Json(json!({ "path": params.path, "text": text })))
}

/// Body is the file's text, verbatim.
async fn write_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileQuery>,
    body: String,
) -> Result<StatusCode> {
    let root = state.workspace_root().await?;
    let file = crate::workspace::resolve(&root, &params.path, false)?;
    tokio::fs::write(&file, body).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct TablesQuery {
    db: Option<String>,
}

async fn tables(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<TablesQuery>,
) -> Result<Json<Vec<TableInfo>>> {
    let record = state.record(&id).await?;
    let db = params.db.unwrap_or_else(|| record.database());
    let connection = state.open(&id, Some(&db)).await?;
    Ok(Json(connection.list_tables(&db).await?))
}

#[derive(Deserialize)]
struct ColumnsQuery {
    db: Option<String>,
    schema: Option<String>,
    table: String,
}

async fn columns(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<ColumnsQuery>,
) -> Result<Json<Vec<ColumnInfo>>> {
    let record = state.record(&id).await?;
    let schema = params
        .schema
        .unwrap_or_else(|| record.kind.default_schema().to_owned());
    let db = params.db.unwrap_or_else(|| record.database());
    let connection = state.open(&id, Some(&db)).await?;
    Ok(Json(
        connection.list_columns(&db, &schema, &params.table).await?,
    ))
}
