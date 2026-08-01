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
        .route("/sources/{id}", delete(remove_source))
        .route("/sources/{id}/status", get(source_status))
        .route("/sources/{id}/databases", get(databases))
        .route("/sources/{id}/tables", get(tables))
        .route("/sources/{id}/columns", get(columns))
        .route("/sources/{id}/schema", get(schema))
        .route("/search", get(search))
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
    if config.id.trim().is_empty() {
        return Err(Error::BadRequest("source id must not be empty".into()));
    }
    if config.instance.is_some() && config.port.is_some() {
        return Err(Error::BadRequest(
            "set either `port` or `instance`, not both".into(),
        ));
    }
    // Reject bad credentials now rather than on the first query, and before the
    // secret goes anywhere near the vault.
    state.connector(config.kind).connect(&config).await?;
    let summary = state.register(config).await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

/// Try the credentials without registering anything — the dialogue's *Test*
/// button. `POST /sources` already validates before saving; this is the same
/// check without the commitment.
async fn test_connection(
    State(state): State<Arc<AppState>>,
    Json(config): Json<SourceConfig>,
) -> Result<Json<Value>> {
    let started = Instant::now();
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

// ------------------------------------------------------------------ workspace

/// The open folder and its `.sql` files. `root: null` when nothing is open.
async fn workspace(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let Some(root) = state.workspace().await else {
        return Ok(Json(
            json!({ "root": null, "files": [], "truncated": false }),
        ));
    };
    let (files, truncated) = crate::workspace::list(&root)?;
    Ok(Json(json!({
        "root": root.to_string_lossy(),
        "files": files,
        "truncated": truncated,
    })))
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
    let db = params.db.unwrap_or_else(|| record.database().to_owned());
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
    let db = params.db.unwrap_or_else(|| record.database().to_owned());
    let connection = state.open(&id, Some(&db)).await?;
    Ok(Json(
        connection.list_columns(&db, &schema, &params.table).await?,
    ))
}
