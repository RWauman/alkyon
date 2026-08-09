//! DuckDB as a federator: make each source available, then run the buffer over
//! them.
//!
//! There are two ways in, and they are not ranked — they trade different things:
//!
//! - **`@import`** runs *your* SQL on the source, in the source's own dialect,
//!   and streams the answer in. Everything you write is pushed down, because the
//!   server is the one running it: aggregates, window functions, hints, a stored
//!   procedure if you like. What travels is the result.
//! - **`@attach`** hands DuckDB the live server and lets its planner generate the
//!   remote query. Projections and filters are pushed down; **aggregations and
//!   joins are not** — see [`attach`] for the measurements. What you get instead
//!   is a whole catalogue you can browse and join without writing SQL per engine.
//!   PostgreSQL, MySQL and SQL Server — the last through a **community**
//!   extension, which is third-party code and says so.
//!
//! Neither path has a row cap. An import used to buffer every cell as a `String`
//! in this process, which is why it did: the limit was really a memory limit
//! wearing a row count. It now streams into DuckDB batch by batch, and DuckDB
//! spills to [`Spill`] when it runs out of memory, so the bound is disk.

pub mod attach;
pub mod excel;
pub mod program;

use std::fmt;
use std::path::PathBuf;

use async_stream::try_stream;
use duckdb::Connection;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{ColumnMeta, LogicalType, RowBatch, BATCH_ROWS};
use crate::state::AppState;
use program::{Import, Program};

/// Bounds on a federated run, passed in rather than read from the environment
/// deep inside the machinery — a global read there is untestable, and made two
/// tests interfere the first time this was written.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    /// How many rows a single import may pull. **`None` by default**: there is no
    /// cap.
    ///
    /// There used to be one, at a million rows, and it was really a memory limit
    /// wearing a row count — every cell was held as a `String` in this process
    /// until the import finished. Rows now go into DuckDB as they arrive and DuckDB
    /// spills to disk, so the number that mattered stopped being the row count.
    ///
    /// Still settable with `ALKYON_IMPORT_MAX_ROWS`, for a shared machine where a
    /// mistyped import should fail rather than fill a disk. Exceeding it is an
    /// error and never a truncation: a join quietly missing half its rows is worse
    /// than a query that failed.
    pub max_import_rows: Option<usize>,
}

impl Limits {
    pub fn from_env() -> Self {
        Self {
            max_import_rows: std::env::var("ALKYON_IMPORT_MAX_ROWS")
                .ok()
                .and_then(|value| value.parse().ok()),
        }
    }
}

/// Fixed scale for decimals coming from a source.
///
/// The result-set metadata of a `select` gives no precision or scale, only "this
/// is a decimal", so a scale has to be chosen. 9 covers money and every
/// measure worth joining on; beyond that, cast explicitly in the import SQL.
const DECIMAL_SCALE: u8 = 9;

/// Extensions linked in by the `parquet` and `json` Cargo features, so reaching
/// them costs nothing and touches no network. CSV needs nothing — it is core.
///
/// Anything absent here is fetched only when a source cannot be read without it —
/// `delta`, `azure`, and the engine extensions an `@attach` needs. Never as a side
/// effect of anything else, and never by a user's query: by the time the buffer
/// runs, loading an extension is refused outright.
const STATIC_EXTENSIONS: &[&str] = &["parquet", "json"];

fn duckdb_type(logical: LogicalType) -> String {
    match logical {
        LogicalType::Bool => "BOOLEAN".into(),
        LogicalType::Int => "BIGINT".into(),
        LogicalType::Float => "DOUBLE".into(),
        LogicalType::Decimal => format!("DECIMAL(38,{DECIMAL_SCALE})"),
        LogicalType::Date => "DATE".into(),
        LogicalType::Time => "TIME".into(),
        LogicalType::Timestamp => "TIMESTAMP".into(),
        LogicalType::TimestampTz => "TIMESTAMPTZ".into(),
        LogicalType::Uuid => "UUID".into(),
        LogicalType::Json => "JSON".into(),
        // Binary arrives as a hex string, and Unknown is whatever the connector
        // could not decode — both stay text rather than guessing.
        LogicalType::Text | LogicalType::Binary | LogicalType::Unknown => "VARCHAR".into(),
    }
}

/// One import, already pulled into memory and ready to be handed to DuckDB.
///
/// Shared with the folder connector, which materialises a spreadsheet the same way
/// a federated `@excel` does — calamine reads it, DuckDB types it.
pub(crate) struct Materialised {
    pub(crate) alias: String,
    pub(crate) columns: Vec<ColumnMeta>,
    /// Every cell as text; DuckDB does the casting.
    pub(crate) rows: Vec<Vec<Option<String>>>,
}

/// One instruction for the thread that owns the DuckDB connection.
///
/// The session is built from the *outside* now: rows arrive from a source on the
/// async side and are appended as they come, rather than being collected into a
/// `Vec<Vec<Option<String>>>` first. That is the whole of what removed the row cap
/// — the cap was never about rows, it was about holding all of them at once.
enum Step {
    /// A view over files DuckDB reads for itself. Nothing crosses this process:
    /// binding it reads a header, and the columns it does not need are never read.
    Scan { alias: String, expression: String },
    /// Start a table. The columns are known from the source's own metadata, which
    /// arrives before its first row.
    Begin {
        alias: String,
        columns: Vec<ColumnMeta>,
    },
    /// Rows for the table most recently begun.
    Rows(Vec<Vec<Option<String>>>),
    /// That table is complete: cast it into shape.
    Seal,
    /// A source failed. The query must not run against a half-filled session, so
    /// this says "stop" rather than simply closing the channel — closing it means
    /// "everything arrived".
    Stop,
}

pub(crate) fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Values reach DuckDB as text and are cast there.
///
/// Doing the conversion in Rust would mean reimplementing date, decimal and
/// interval parsing for every dialect; DuckDB's is better tested than anything
/// written here would be. It is also the only way a `numeric` keeps its digits,
/// since it already travels as a string to avoid `f64` rounding.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Pull an import's rows from its source and push them at DuckDB as they arrive.
///
/// Nothing accumulates here. The channel is short, so a source that outruns the
/// appender is made to wait rather than filling this process with `String`s —
/// which is what the row cap used to be protecting against.
async fn stream_query(
    state: &AppState,
    steps: &tokio::sync::mpsc::Sender<Step>,
    alias: &str,
    source: &str,
    database: Option<&str>,
    sql: &str,
    cap: Option<usize>,
) -> Result<()> {
    let connection = state.open(source, database).await?;
    let mut batches = connection.execute(sql);

    let mut began = false;
    let mut rows = 0usize;

    while let Some(batch) = batches.next().await {
        match batch? {
            RowBatch::Columns(meta) => {
                if began {
                    // A batch of several statements has no single shape to import.
                    return Err(Error::BadRequest(format!(
                        "@import {alias}: the SQL returned more than one result set"
                    )));
                }
                began = true;
                send(
                    steps,
                    Step::Begin {
                        alias: alias.to_owned(),
                        columns: meta.as_ref().clone(),
                    },
                )
                .await?;
            }
            RowBatch::Rows(batch) => {
                rows += batch.len();
                if let Some(cap) = cap.filter(|cap| rows > *cap) {
                    return Err(Error::BadRequest(format!(
                        "@import {alias}: more than {cap} rows, which is the ceiling \
                         ALKYON_IMPORT_MAX_ROWS sets. Narrow the import, or raise it — there is \
                         no cap unless one is asked for."
                    )));
                }
                let text = batch
                    .iter()
                    .map(|row| row.iter().map(as_text).collect::<Vec<_>>())
                    .collect();
                send(steps, Step::Rows(text)).await?;
            }
            RowBatch::Affected(_) => {}
        }
    }

    if !began {
        return Err(Error::BadRequest(format!(
            "@import {alias}: the SQL returned no result set"
        )));
    }
    send(steps, Step::Seal).await?;
    tracing::info!(alias, rows, "streamed an import");
    Ok(())
}

/// Hand a step to the DuckDB thread.
///
/// A closed channel means the worker has already failed, and its error is the one
/// worth reporting — so this says so plainly and lets the caller reconcile.
async fn send(steps: &tokio::sync::mpsc::Sender<Step>, step: Step) -> Result<()> {
    steps
        .send(step)
        .await
        .map_err(|_| Error::Federated("the DuckDB session ended early".into()))
}

/// Turn `<folder source>/<pattern>` into a scan DuckDB can bind.
///
/// The source must be a folder or file one: everything else has SQL to run, and
/// an import without SQL is how you say you want the files themselves.
async fn prepare_scan(
    state: &AppState,
    alias: &str,
    source: &str,
    pattern: &str,
) -> Result<(String, Sandbox)> {
    let record = state.record(source).await?;
    // Azure storage is excluded along with the servers, and for the same reason
    // in reverse: its path names a container, not a folder anyone can point a
    // glob at. `@import x = azure-source : select …` is how it is imported.
    if !record.kind.is_local_files() {
        return Err(Error::BadRequest(format!(
            "@import {alias}: `{source}` is a {:?} source, so it needs SQL to run — \
             write `@import {alias} = {source} : <sql>`. Only a folder or file source \
             can be imported by path.",
            record.kind
        )));
    }
    let path = record.path.as_deref().unwrap_or_default();
    let root = crate::workspace::resolve_root(path)?;

    // An empty pattern means the source is the file. Otherwise the pattern is
    // relative to the folder — `parse_files_import` already refused `..` and
    // absolute paths, and `allowed_directories` refuses whatever slips past.
    let target = if pattern.is_empty() {
        if root.is_dir() {
            return Err(Error::BadRequest(format!(
                "@import {alias}: `{source}` is a folder, so say which files — \
                 `{source}/*.parquet`"
            )));
        }
        root.to_string_lossy().replace('\\', "/")
    } else {
        format!("{}/{pattern}", root.to_string_lossy().replace('\\', "/"))
    };

    // The reader comes from the pattern's own extension, so a glob picks the same
    // one its files would. `*` has no extension of its own to go by.
    let reader =
        crate::connectors::files::reader_for(std::path::Path::new(&target)).ok_or_else(|| {
            Error::BadRequest(format!(
                "@import {alias}: `{pattern}` names no format alkyon reads ({}). \
                 A spreadsheet goes through `@excel`.",
                crate::connectors::files::readable_extensions()
            ))
        })?;

    Ok((
        format!("{reader}({})", quote_literal(&target)),
        // A folder source grants its folder — a glob has to be listed before it
        // can be opened. A file source grants only that file, so importing it
        // here is no wider than querying it directly.
        if root.is_dir() {
            Sandbox::directory(root)
        } else {
            Sandbox::file(root)
        },
    ))
}

fn materialise_excel(
    alias: &str,
    root: Option<&PathBuf>,
    path: &str,
    sheet: Option<&str>,
) -> Result<Materialised> {
    let root = root.ok_or_else(|| {
        Error::BadRequest(format!(
            "@excel {alias}: no folder is open, so `{path}` cannot be resolved"
        ))
    })?;
    // Same confinement as the workspace file API, minus the `.sql` rule.
    let resolved = resolve_data_file(root, path)?;
    let sheet = excel::read(&resolved, sheet)?;
    Ok(Materialised {
        alias: alias.to_owned(),
        columns: sheet.columns,
        rows: sheet.rows,
    })
}

/// Resolve a data file inside `root`, refusing anything that leaves it.
fn resolve_data_file(root: &std::path::Path, relative: &str) -> Result<PathBuf> {
    use std::path::Component;

    let candidate = std::path::Path::new(relative);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::BadRequest(format!(
            "`{relative}` must be a plain relative path inside the open folder"
        )));
    }
    let root = root
        .canonicalize()
        .map_err(|e| Error::BadRequest(format!("the open folder is unreadable: {e}")))?;
    let real = root
        .join(candidate)
        .canonicalize()
        .map_err(|e| Error::BadRequest(format!("cannot resolve `{relative}`: {e}")))?;
    if !real.starts_with(&root) {
        return Err(Error::BadRequest(format!(
            "`{relative}` is outside the open folder"
        )));
    }
    Ok(real)
}

/// What a DuckDB session is allowed to touch on the filesystem.
///
/// Both lists are *exceptions*: everything else is refused because external
/// access is off. `files` exists so a source pointing at one spreadsheet does not
/// have to be granted the directory it happens to sit in.
#[derive(Debug, Default, Clone)]
pub struct Sandbox {
    pub directories: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
}

impl Sandbox {
    pub fn directory(root: PathBuf) -> Self {
        Self {
            directories: vec![root],
            files: Vec::new(),
        }
    }

    pub fn file(path: PathBuf) -> Self {
        Self {
            directories: Vec::new(),
            files: vec![path],
        }
    }

    /// Take in another grant. A federated buffer may name several folder sources,
    /// and the session needs every one of them.
    pub fn absorb(&mut self, other: Sandbox) {
        for directory in other.directories {
            if !self.directories.contains(&directory) {
                self.directories.push(directory);
            }
        }
        for file in other.files {
            if !self.files.contains(&file) {
                self.files.push(file);
            }
        }
    }
}

/// The columns DuckDB reports for a relation, whatever built it.
///
/// A `DESCRIBE` and nothing more, shared because two connectors ask the same
/// question of the same engine: a folder source about a `read_csv`, a MongoDB
/// source about a view over NDJSON. Neither has constraints to report — a
/// document may be missing any field, and nothing is a key.
pub(crate) fn describe_view(
    connection: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<crate::model::ColumnInfo>> {
    let sql = format!(
        "DESCRIBE SELECT * FROM {}.{}",
        quote_identifier(schema),
        quote_identifier(relation)
    );
    let mut statement = connection.prepare(&sql).map_err(federated)?;
    let mut rows = statement.query([]).map_err(federated)?;

    let mut columns = Vec::new();
    let mut ordinal = 1;
    while let Some(row) = rows.next().map_err(federated)? {
        columns.push(crate::model::ColumnInfo {
            name: row.get::<_, String>(0).map_err(federated)?,
            ordinal,
            data_type: row.get::<_, String>(1).map_err(federated)?.to_lowercase(),
            nullable: true,
            is_primary_key: false,
            default: None,
        });
        ordinal += 1;
    }
    Ok(columns)
}

/// Install and load an extension that is not linked in.
///
/// `LOAD` first: after the first time there is nothing to fetch. `INSTALL`
/// reaches `extensions.duckdb.org` and writes to DuckDB's own extension
/// directory, which is why it happens for a source that cannot be read without it
/// and never as a side effect of anything else.
fn fetch_extension(connection: &Connection, name: &str, repository: Option<&str>) -> Result<()> {
    if connection.execute_batch(&format!("LOAD {name};")).is_ok() {
        return Ok(());
    }
    let from = match repository {
        // Third-party native code in this process. It happens because a buffer
        // asked for it, and it is said out loud rather than slipped in.
        Some(repository) => {
            tracing::warn!(
                extension = name,
                repository,
                "installing a community DuckDB extension — third-party code, not DuckDB's own"
            );
            format!(" FROM {repository}")
        }
        None => {
            tracing::info!(extension = name, "installing a DuckDB extension");
            String::new()
        }
    };
    connection
        .execute_batch(&format!("INSTALL {name}{from}; LOAD {name};"))
        .map_err(|e| {
            let note = if repository.is_some() {
                " It is a community extension, so it may not be published for this DuckDB \
                 version and platform."
            } else {
                ""
            };
            Error::Federated(format!(
                "cannot install DuckDB's `{name}` extension, which is what reads this source: \
                 {e}{note}"
            ))
        })
}

/// What an Azure storage session needs to reach the account, and nothing else.
#[derive(Clone)]
pub struct AzureAccess {
    /// The storage account the secret is for — `contoso`, or `onelake`.
    pub account: String,
    /// A bearer token for `https://storage.azure.com`, which is exactly what the
    /// Entra sign-in mints.
    pub token: String,
    /// What the explorer shows where a server shows a database: the container and
    /// how far into it, which is what tells one Azure source from another.
    pub catalogue: String,
}

impl fmt::Debug for AzureAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureAccess")
            .field("account", &self.account)
            .field("catalogue", &self.catalogue)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Open a DuckDB that can read one Azure storage account, and **no local file**.
///
/// The confinement is the same idea as [`open_duckdb_in`]'s, turned inside out.
/// There, external access is off and a directory is the exception; here external
/// access has to stay on — an extension and a network read both need it — so the
/// local filesystem is shut instead:
///
/// ```text
/// read a local file            → File system LocalFileSystem has been disabled
/// reopen the local filesystem  → the configuration has been locked
/// ```
///
/// Which makes this session *tighter* than a folder source's: that one can read a
/// directory, this one can read nothing on the machine at all.
///
/// `enable_external_access` is deliberately never mentioned. It cannot be turned
/// on after startup — DuckDB refuses with "Cannot enable external access while
/// database is running" — so the only way to have it is to leave the default
/// alone, and the only thing that matters is what is closed afterwards.
pub fn open_duckdb_azure(
    access: &AzureAccess,
    schema: Option<&str>,
    extensions: &[&str],
) -> Result<Connection> {
    let connection = Connection::open_in_memory().map_err(federated)?;

    fetch_extension(&connection, "azure", None)?;
    for extension in extensions {
        fetch_extension(&connection, extension, None)?;
    }

    let mut setup = String::new();
    if let Some(schema) = schema {
        setup.push_str(&format!(
            "CREATE SCHEMA IF NOT EXISTS {};\nSET search_path = {};\n",
            quote_identifier(schema),
            quote_literal(schema)
        ));
    }

    // A secret rather than a setting: it is scoped to the account named here, so
    // the session can reach that one and no other. The token is the one the
    // sign-in already holds — `PROVIDER access_token` wants an audience of
    // `https://storage.azure.com`, which is what it was minted for.
    setup.push_str(&format!(
        "CREATE SECRET alkyon_azure (TYPE azure, PROVIDER access_token, \
         ACCESS_TOKEN {}, ACCOUNT_NAME {});\n",
        quote_literal(&access.token),
        quote_literal(&access.account),
    ));

    setup.push_str(
        "SET disabled_filesystems = 'LocalFileSystem';\n\
         SET allow_community_extensions = false;\n\
         SET allow_unsigned_extensions = false;\n\
         SET autoinstall_known_extensions = false;\n\
         SET autoload_known_extensions = false;\n\
         SET lock_configuration = true;\n",
    );

    connection.execute_batch(&setup).map_err(|e| {
        // Never the setup text: it carries the token.
        Error::Federated(format!("could not secure the Azure DuckDB session: {e}"))
    })?;
    Ok(connection)
}

/// Open an in-memory DuckDB, confine it, then freeze the configuration.
///
/// This matters more than it looks: DuckDB can read and write arbitrary files and
/// load extensions, so an unconfined instance behind an HTTP endpoint is arbitrary
/// file access and arbitrary code loading. `lock_configuration` is what stops a
/// user query from undoing any of it.
pub fn open_duckdb(sandbox: &Sandbox) -> Result<Connection> {
    open_duckdb_in(sandbox, None)
}

/// [`open_duckdb`], with `schema` created and made the default one.
///
/// It has to happen here rather than at the call site: `search_path` is a
/// configuration option, so setting it after `lock_configuration` is refused, and
/// the schema has to exist before the search path may name it.
pub fn open_duckdb_in(sandbox: &Sandbox, schema: Option<&str>) -> Result<Connection> {
    open_duckdb_needing(sandbox, schema, &[])
}

/// [`open_duckdb_in`], having first fetched an extension the source cannot be read
/// without — `delta`, today.
///
/// **Before** external access is turned off, which is the only order that works:
/// `LOAD` needs it, and it cannot be turned back on once a database is running.
/// Measured: the extension keeps working afterwards — a `delta_scan` still runs
/// with the door shut — and a file outside the grant is still refused. So a Delta
/// table costs no confinement.
pub fn open_duckdb_needing(
    sandbox: &Sandbox,
    schema: Option<&str>,
    extensions: &[&str],
) -> Result<Connection> {
    open_duckdb_with(Setup {
        sandbox,
        schema,
        extensions,
        attachments: &[],
        spill: None,
    })
}

/// Everything a confined DuckDB session may be opened with.
///
/// A struct rather than five positional arguments, because the *order* of what
/// follows is the only thing that makes any of it work, and a caller has no
/// business choosing it.
pub struct Setup<'a> {
    pub sandbox: &'a Sandbox,
    /// Created and put on the search path, so a bare name resolves there.
    pub schema: Option<&'a str>,
    /// Fetched before the door shuts. `INSTALL` reaches the network.
    pub extensions: &'a [&'a str],
    /// Servers to attach. Their extensions are fetched from
    /// [`attach::Engine::extension`], so they need not be listed above.
    pub attachments: &'a [attach::Attachment],
    /// Where DuckDB may spill when it runs out of memory. Without one, an
    /// oversized query is an out-of-memory error instead.
    pub spill: Option<&'a std::path::Path>,
}

/// Open an in-memory DuckDB, give it what [`Setup`] allows, then freeze it.
///
/// The order is the whole design, and every step of it was measured:
///
/// 1. **Extensions and attachments first**, while external access is still on:
///    `INSTALL` fetches over the network and `ATTACH` opens a connection, and
///    external access cannot be turned back on once a database is running.
/// 2. **Then the grants** — `allowed_directories` and `allowed_paths` — which
///    DuckDB refuses to change after external access is off.
/// 3. **Then the door**, and then the lock.
///
/// The measurement that matters: an attachment made in step 1 keeps working after
/// step 3. A remote `count`, a pushed-down filter, even a self-join that needs more
/// connections than the warm one — all still answer, while `read_csv` on an
/// ungranted local file is refused. So attaching a server costs no confinement,
/// exactly as loading an extension does not.
pub fn open_duckdb_with(setup: Setup<'_>) -> Result<Connection> {
    let Setup {
        sandbox,
        schema,
        extensions,
        attachments,
        spill,
    } = setup;
    let connection = Connection::open_in_memory().map_err(federated)?;
    for extension in extensions {
        fetch_extension(&connection, extension, None)?;
    }
    for attachment in attachments {
        fetch_extension(
            &connection,
            attachment.engine.extension(),
            attachment.engine.repository(),
        )?;
        for statement in attachment.statements() {
            connection.execute_batch(statement).map_err(|e| {
                // Never the statement: the secret is in it. The alias is enough to
                // say which `@attach` line went wrong.
                Error::Federated(format!("@attach {}: {e}", attachment.alias))
            })?;
        }
        tracing::info!(
            alias = %attachment.alias,
            engine = attachment.engine.extension(),
            "attached a server"
        );
    }

    // Before anything is locked down, and best effort: the `parquet` and `json`
    // Cargo features link these in, in which case they may already be registered
    // and `LOAD` is redundant. What proves they work is a query, not this call.
    for extension in STATIC_EXTENSIONS {
        if let Err(e) = connection.execute_batch(&format!("LOAD {extension};")) {
            tracing::debug!(extension, error = %e, "extension not loaded explicitly");
        }
    }

    // Both lists have to be set *before* external access is turned off: DuckDB
    // refuses to change them afterwards ("Cannot change allowed_paths when
    // enable_external_access is disabled"), which is exactly the point.
    let mut setup = String::new();
    if let Some(schema) = schema {
        setup.push_str(&format!(
            "CREATE SCHEMA IF NOT EXISTS {};\nSET search_path = {};\n",
            quote_identifier(schema),
            quote_literal(schema)
        ));
    }
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| quote_literal(&path.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !sandbox.directories.is_empty() {
        setup.push_str(&format!(
            "SET allowed_directories = [{}];\n",
            list(&sandbox.directories)
        ));
        // No `file_search_path`: with external access disabled, the permission
        // check happens on the path as written, before any search path applies, so
        // a relative one is refused however the search path is set. Files are
        // addressed through the `${folder}` placeholder instead — one rule for
        // reads and writes alike. See [`substitute`].
    }
    if !sandbox.files.is_empty() {
        setup.push_str(&format!(
            "SET allowed_paths = [{}];\n",
            list(&sandbox.files)
        ));
    }
    if let Some(spill) = spill {
        // Somewhere to put what will not fit in memory. Without this an oversized
        // query is an "Out of Memory Error" and nothing else; with it, DuckDB
        // writes temporary files and finishes. Measured under a 150 MB limit: a
        // million appended rows landed in seven spill files and counted exactly.
        //
        // Not part of the grant, and it does not have to be: the temp directory is
        // DuckDB's own bookkeeping, and it keeps working with external access off.
        setup.push_str(&format!(
            "SET temp_directory = {};\n",
            quote_literal(&spill.to_string_lossy())
        ));
    }

    // This is the line that actually enforces anything.
    //
    // `allowed_directories` on its own does nothing: it is the *exception* list.
    // Leaving external access enabled and merely listing directories left every
    // file on the machine readable — which is what the confinement test caught.
    setup.push_str("SET enable_external_access = false;\n");
    // Nothing may be *fetched*: that would be remote code, and it would break the
    // offline promise besides. Autoload is off for the same reason — an implicit
    // load of something not linked in would reach for the network.
    setup.push_str(
        "SET allow_community_extensions = false;\n\
         SET allow_unsigned_extensions = false;\n\
         SET autoinstall_known_extensions = false;\n\
         SET autoload_known_extensions = false;\n\
         SET lock_configuration = true;\n",
    );

    connection
        .execute_batch(&setup)
        .map_err(|e| Error::Federated(format!("could not secure the DuckDB session: {e}")))?;
    Ok(connection)
}

fn federated(e: duckdb::Error) -> Error {
    Error::Federated(e.to_string())
}

/// The token that stands for the open folder inside a path literal.
const FOLDER_TOKEN: &str = "${folder}";

/// Expand `${folder}` to the open folder's path.
///
/// This is how every file is addressed. A relative path cannot work: confinement
/// checks the path as written, before DuckDB's search path is consulted, so a
/// relative read is refused no matter how the search path is set — and a relative
/// `COPY … TO` would land wherever the alkyon process happens to be running.
/// `COPY … TO` takes a string literal rather than an expression, so a DuckDB
/// variable cannot stand in either.
///
/// A documented placeholder is the honest answer: no SQL parsing, no mutating the
/// process-wide working directory behind the user's back, and one rule for input
/// and output.
fn substitute(sql: &str, root: Option<&PathBuf>) -> Result<String> {
    if !sql.contains(FOLDER_TOKEN) {
        return Ok(sql.to_owned());
    }
    let root =
        root.ok_or_else(|| Error::BadRequest(format!("{FOLDER_TOKEN} needs a folder to be open")))?;

    // Forward slashes so the result reads the same on every platform, and doubled
    // quotes so a path containing one cannot end the literal it sits in.
    let path = root
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "''");
    Ok(sql.replace(FOLDER_TOKEN, &path))
}

/// A table being filled from a source, one batch at a time.
///
/// Two tables on purpose, and this is where that happens: rows land in an
/// all-VARCHAR staging table, and [`seal`] `CAST`s them into the real one. DuckDB
/// parses the text, so decimals keep their digits and dates parse without a
/// hand-written converter per dialect.
struct Staging<'a> {
    alias: String,
    columns: Vec<ColumnMeta>,
    table: String,
    appender: duckdb::Appender<'a>,
    rows: usize,
}

/// Create the staging table for `alias` and open an appender on it.
fn begin<'a>(
    connection: &'a Connection,
    alias: &str,
    columns: Vec<ColumnMeta>,
) -> Result<Staging<'a>> {
    let table = format!("{alias}__alkyon_raw");
    let placeholders = (0..columns.len())
        .map(|index| format!("c{index} VARCHAR"))
        .collect::<Vec<_>>()
        .join(", ");

    // Explicitly in `main`: the appender resolves a bare table name there whatever
    // the search path says, and a folder source sets the search path to `public`.
    connection
        .execute_batch(&format!(
            "CREATE TABLE main.{} ({placeholders});",
            quote_identifier(&table)
        ))
        .map_err(federated)?;

    let appender = connection.appender(&table).map_err(federated)?;
    Ok(Staging {
        alias: alias.to_owned(),
        columns,
        table,
        appender,
        rows: 0,
    })
}

impl Staging<'_> {
    fn push(&mut self, batch: &[Vec<Option<String>>]) -> Result<()> {
        for row in batch {
            // `&dyn ToSql` over Option<String> gives NULL for None.
            let values: Vec<&dyn duckdb::ToSql> =
                row.iter().map(|cell| cell as &dyn duckdb::ToSql).collect();
            self.appender.append_row(&values[..]).map_err(federated)?;
        }
        self.rows += batch.len();
        Ok(())
    }
}

/// Type the staged rows into the table the query will see, and drop the staging.
fn seal(connection: &Connection, mut staging: Staging<'_>) -> Result<usize> {
    staging.appender.flush().map_err(federated)?;
    drop(staging.appender);

    let projection = staging
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let target = duckdb_type(column.logical);
            if target == "VARCHAR" {
                format!("c{index} AS {}", quote_identifier(&column.name))
            } else {
                format!(
                    "CAST(c{index} AS {target}) AS {}",
                    quote_identifier(&column.name)
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    connection
        .execute_batch(&format!(
            "CREATE TABLE {alias} AS SELECT {projection} FROM main.{staging};\n\
             DROP TABLE main.{staging};",
            alias = quote_identifier(&staging.alias),
            staging = quote_identifier(&staging.table),
        ))
        .map_err(|e| {
            Error::Federated(format!(
                "@import {}: could not type the imported rows: {e}",
                staging.alias
            ))
        })?;
    Ok(staging.rows)
}

/// Create `alias` in DuckDB from rows already held in memory.
///
/// The one caller left is the folder connector's spreadsheet path: calamine reads
/// a whole sheet before it can report a column, so there is nothing to stream.
pub(crate) fn load(connection: &Connection, table: &Materialised) -> Result<()> {
    let mut staging = begin(connection, &table.alias, table.columns.clone())?;
    staging.push(&table.rows)?;
    seal(connection, staging)?;
    Ok(())
}

/// Build the session from the steps the feeder sends, until it says it is done.
///
/// Returns whether the query should run: a [`Step::Stop`] means a source failed
/// part way, and answering from a half-filled session would be worse than any
/// error message.
fn build(connection: &Connection, steps: &mut tokio::sync::mpsc::Receiver<Step>) -> Result<bool> {
    let mut open: Option<Staging> = None;

    while let Some(step) = steps.blocking_recv() {
        match step {
            Step::Scan { alias, expression } => {
                // A view, not a table: binding it reads the file's header and
                // nothing else until the query asks for rows.
                connection
                    .execute_batch(&format!(
                        "CREATE VIEW {} AS SELECT * FROM {expression};",
                        quote_identifier(&alias)
                    ))
                    .map_err(|e| Error::Federated(format!("@import {alias}: {e}")))?;
            }
            Step::Begin { alias, columns } => {
                open = Some(begin(connection, &alias, columns)?);
            }
            Step::Rows(batch) => match open.as_mut() {
                Some(staging) => staging.push(&batch)?,
                None => {
                    return Err(Error::Federated(
                        "rows arrived before the table they belong to".into(),
                    ))
                }
            },
            Step::Seal => {
                if let Some(staging) = open.take() {
                    let alias = staging.alias.clone();
                    let rows = seal(connection, staging)?;
                    tracing::debug!(alias = %alias, rows, "loaded an import");
                }
            }
            Step::Stop => return Ok(false),
        }
    }
    Ok(true)
}

/// Where DuckDB writes what will not fit in memory, removed when the query ends.
///
/// This is what makes an uncapped import a promise rather than a hope: without a
/// temp directory, an in-memory DuckDB that runs out of memory has nowhere to go
/// and fails.
struct Spill(PathBuf);

impl Spill {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("alkyon-spill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        Ok(Spill(path))
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        // Best effort: a leftover directory is untidy, not wrong, and there is
        // nothing useful to do with the error here.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `sql` in DuckDB, pushing batches down `sink`.
pub(crate) fn run(
    connection: &Connection,
    sql: &str,
    sink: &tokio::sync::mpsc::Sender<Result<RowBatch>>,
) -> Result<()> {
    let mut statement = connection.prepare(sql).map_err(federated)?;
    let mut rows = statement.query([]).map_err(federated)?;

    let mut columns: Option<std::sync::Arc<Vec<ColumnMeta>>> = None;
    let mut buffered: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);

    while let Some(row) = rows.next().map_err(federated)? {
        if columns.is_none() {
            let statement = row.as_ref();
            let meta: Vec<ColumnMeta> = statement
                .column_names()
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let kind = statement.column_type(index);
                    ColumnMeta {
                        name: name.clone(),
                        type_name: kind.to_string().to_lowercase(),
                        logical: logical_of(&duckdb::types::Type::from(&kind)),
                    }
                })
                .collect();
            let meta = std::sync::Arc::new(meta);
            columns = Some(meta.clone());
            if sink.blocking_send(Ok(RowBatch::Columns(meta))).is_err() {
                return Ok(());
            }
        }

        let width = columns.as_ref().map_or(0, |c| c.len());
        let mut values = Vec::with_capacity(width);
        for index in 0..width {
            values.push(cell_to_json(row, index));
        }
        buffered.push(values);

        if buffered.len() >= BATCH_ROWS {
            let batch = std::mem::take(&mut buffered);
            if sink.blocking_send(Ok(RowBatch::Rows(batch))).is_err() {
                return Ok(());
            }
        }
    }

    if columns.is_none() {
        // A statement that returns nothing at all, e.g. `COPY … TO`.
        let _ = sink.blocking_send(Ok(RowBatch::Affected(0)));
    } else if !buffered.is_empty() {
        let _ = sink.blocking_send(Ok(RowBatch::Rows(buffered)));
    }
    Ok(())
}

/// What a DuckDB result column holds, in the connector-independent vocabulary.
///
/// Not cosmetic: a folder source answers through this path, and importing one
/// into a federated query re-types every column from what is reported here. Left
/// as `Unknown` — as it was while DuckDB could only ever be the *last* engine in
/// the chain — every imported column landed as VARCHAR and `sum()` stopped
/// working on a perfectly good decimal.
fn logical_of(kind: &duckdb::types::Type) -> LogicalType {
    use duckdb::types::Type;
    match kind {
        Type::Boolean => LogicalType::Bool,
        Type::TinyInt
        | Type::SmallInt
        | Type::Int
        | Type::BigInt
        | Type::UTinyInt
        | Type::USmallInt
        | Type::UInt
        | Type::UBigInt => LogicalType::Int,
        Type::Float | Type::Double => LogicalType::Float,
        // HUGEINT is 128-bit: wider than i64, so it travels as exact digits
        // rather than being rounded into a JSON number.
        Type::Decimal | Type::HugeInt | Type::UHugeInt => LogicalType::Decimal,
        Type::Date32 => LogicalType::Date,
        Type::Time64 => LogicalType::Time,
        Type::Timestamp => LogicalType::Timestamp,
        Type::Text | Type::Enum => LogicalType::Text,
        Type::Blob => LogicalType::Binary,
        // Intervals, lists, structs, maps and unions are rendered by DuckDB's own
        // formatter; there is no scalar type to promise here.
        _ => LogicalType::Unknown,
    }
}

/// DuckDB values as JSON, matching how the native connectors render theirs.
fn cell_to_json(row: &duckdb::Row<'_>, index: usize) -> Value {
    use duckdb::types::ValueRef;

    match row.get_ref(index) {
        Err(e) => Value::String(format!("<decode error: {e}>")),
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Boolean(b)) => Value::Bool(b),
        Ok(ValueRef::TinyInt(v)) => Value::from(v),
        Ok(ValueRef::SmallInt(v)) => Value::from(v),
        Ok(ValueRef::Int(v)) => Value::from(v),
        Ok(ValueRef::BigInt(v)) => Value::from(v),
        Ok(ValueRef::UTinyInt(v)) => Value::from(v),
        Ok(ValueRef::USmallInt(v)) => Value::from(v),
        Ok(ValueRef::UInt(v)) => Value::from(v),
        Ok(ValueRef::UBigInt(v)) => Value::from(v),
        Ok(ValueRef::Float(v)) => Value::from(v),
        Ok(ValueRef::Double(v)) => Value::from(v),
        Ok(ValueRef::Text(bytes)) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        Ok(ValueRef::Blob(bytes)) => Value::String(format!("\\x{}", hex(bytes))),
        // 128-bit, and the type `sum()` over any integer column returns — so this
        // is not an exotic case, it is the most ordinary aggregate there is.
        // Without these arms it fell through to the debug formatter and a total of
        // sixty was displayed as `HugeInt(60)`.
        Ok(ValueRef::HugeInt(v)) => hugeint(i64::try_from(v).ok(), v.to_string()),
        Ok(ValueRef::UHugeInt(v)) => hugeint(i64::try_from(v).ok(), v.to_string()),
        // Decimals, dates, intervals, lists, structs.
        Ok(other) => value_to_json(&duckdb::types::Value::from(other)),
    }
}

/// A DuckDB value as JSON, including the ones that are not scalars.
///
/// This used to be `format!("{value:?}")`, which is Rust's derived `Debug` and not
/// a rendering of anything: a date came out as `Date32(20455)`, a total as
/// `Decimal(Decimal { width: 38, scale: 2, value: 41948375 })`, and a struct as
/// `Struct(OrderedMap([…]))`. It went unnoticed because the formats alkyon reads
/// mostly carry dates as text — and then a MongoDB source, whose documents are
/// full of timestamps and sub-documents, made it the first thing you saw.
///
/// The rules:
///
/// - a **decimal** is its exact digits as a string, the way `numeric` already
///   travels from PostgreSQL. Turning it into a float to make it a JSON number is
///   the one thing this codebase refuses to do to a number
/// - a **date, time or timestamp** is ISO-8601 text, which is what every native
///   connector sends
/// - a **list, array or struct** is a JSON array or object, so the grid can show
///   structure rather than a debug dump — and `unnest` and `.field` still work in
///   SQL either way
fn value_to_json(value: &duckdb::types::Value) -> Value {
    use duckdb::types::Value as Duck;

    match value {
        Duck::Null => Value::Null,
        Duck::Boolean(b) => Value::Bool(*b),
        Duck::TinyInt(v) => Value::from(*v),
        Duck::SmallInt(v) => Value::from(*v),
        Duck::Int(v) => Value::from(*v),
        Duck::BigInt(v) => Value::from(*v),
        Duck::UTinyInt(v) => Value::from(*v),
        Duck::USmallInt(v) => Value::from(*v),
        Duck::UInt(v) => Value::from(*v),
        Duck::UBigInt(v) => Value::from(*v),
        Duck::Float(v) => Value::from(*v),
        Duck::Double(v) => Value::from(*v),
        Duck::HugeInt(v) => hugeint(i64::try_from(*v).ok(), v.to_string()),
        Duck::UHugeInt(v) => hugeint(i64::try_from(*v).ok(), v.to_string()),
        Duck::Text(s) | Duck::Enum(s) => Value::String(s.clone()),
        Duck::Blob(bytes) | Duck::Geometry(bytes) => Value::String(format!("\\x{}", hex(bytes))),
        // `Display`, not `Debug`: the scaled integer and its scale, rendered.
        Duck::Decimal(d) => Value::String(d.to_string()),
        Duck::Date32(days) => Value::String(date_text(*days)),
        Duck::Timestamp(unit, v) => Value::String(timestamp_text(*unit, *v)),
        Duck::Time64(unit, v) => Value::String(time_text(*unit, *v)),
        Duck::Interval {
            months,
            days,
            nanos,
        } => Value::String(interval_text(*months, *days, *nanos)),
        Duck::List(items) | Duck::Array(items) => {
            Value::Array(items.iter().map(value_to_json).collect())
        }
        Duck::Struct(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), value_to_json(value)))
                .collect(),
        ),
        // A map's keys need not be strings, so it cannot be a JSON object without
        // inventing something. Pairs say what it is.
        Duck::Map(entries) => Value::Array(
            entries
                .iter()
                .map(|(key, value)| {
                    Value::Object(
                        [
                            ("key".to_owned(), value_to_json(key)),
                            ("value".to_owned(), value_to_json(value)),
                        ]
                        .into_iter()
                        .collect(),
                    )
                })
                .collect(),
        ),
        Duck::Union(inner) => value_to_json(inner),
        // The enum is `#[non_exhaustive]`: a DuckDB upgrade may add a type this
        // was never told about. Debug output is a poor cell, but it is visible and
        // reports what the value is, which beats an empty one — and the compiler
        // stops warning about exactly the case we would want to be warned about.
        other => Value::String(format!("{other:?}")),
    }
}

/// Seconds and nanoseconds since the epoch, from one of DuckDB's four precisions.
fn split_unit(unit: duckdb::types::TimeUnit, value: i64) -> (i64, u32) {
    use duckdb::types::TimeUnit;
    let per_second: i64 = match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    };
    // Euclidean, so a timestamp before 1970 does not round the wrong way.
    let seconds = value.div_euclid(per_second);
    let fraction = value.rem_euclid(per_second) * (1_000_000_000 / per_second);
    (seconds, fraction as u32)
}

fn date_text(days: i32) -> String {
    match chrono::DateTime::from_timestamp(i64::from(days) * 86_400, 0) {
        Some(when) => when.date_naive().to_string(),
        // Outside what a calendar can hold: the number is better than nothing.
        None => days.to_string(),
    }
}

fn timestamp_text(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let (seconds, nanos) = split_unit(unit, value);
    match chrono::DateTime::from_timestamp(seconds, nanos) {
        Some(when) => when
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string(),
        None => value.to_string(),
    }
}

fn time_text(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let (seconds, nanos) = split_unit(unit, value);
    match u32::try_from(seconds)
        .ok()
        .and_then(|s| chrono::NaiveTime::from_num_seconds_from_midnight_opt(s, nanos))
    {
        Some(time) => time.format("%H:%M:%S%.f").to_string(),
        None => value.to_string(),
    }
}

/// Months, days and nanoseconds, said in words rather than as three numbers.
///
/// Months and days stay separate from the clock part on purpose: a month is not
/// 30 days and a day is not always 24 hours, and flattening them would be
/// inventing an answer.
fn interval_text(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    if months != 0 {
        parts.push(format!("{months} month{}", plural(months.into())));
    }
    if days != 0 {
        parts.push(format!("{days} day{}", plural(days.into())));
    }
    if nanos != 0 || parts.is_empty() {
        let (seconds, fraction) = (nanos.div_euclid(1_000_000_000), nanos.rem_euclid(1_000_000_000));
        let (hours, rest) = (seconds / 3_600, seconds % 3_600);
        let clock = format!("{hours:02}:{:02}:{:02}", rest / 60, rest % 60);
        parts.push(if fraction == 0 {
            clock
        } else {
            format!("{clock}.{:09}", fraction).trim_end_matches('0').to_owned()
        });
    }
    parts.join(" ")
}

fn plural(n: i64) -> &'static str {
    if n.abs() == 1 {
        ""
    } else {
        "s"
    }
}

/// A JSON number while the value fits one, exact digits as text beyond that.
///
/// JSON numbers stop at 64 bits, and silently rounding a 128-bit total through
/// `f64` is the one thing this codebase refuses to do to a number.
fn hugeint(fits: Option<i64>, digits: String) -> Value {
    match fits {
        Some(value) => Value::from(value),
        None => Value::String(digits),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Materialise the program's imports, then stream the buffer's result.
pub fn execute<'a>(
    state: &'a AppState,
    program: Program,
    limits: Limits,
) -> BoxStream<'a, Result<RowBatch>> {
    Box::pin(try_stream! {
        let root = state.workspace().await;
        let mut sandbox = match &root {
            Some(root) => Sandbox::directory(root.clone()),
            None => Sandbox::default(),
        };

        // First pass: everything that has to be settled *before* DuckDB opens,
        // because the session's grants and its attachments cannot be changed once
        // it is locked. No rows are read here.
        let mut scans = Vec::new();
        let mut attachments = Vec::new();
        for import in &program.imports {
            match import {
                Import::Files { alias, source, pattern } => {
                    let (expression, grant) = prepare_scan(state, alias, source, pattern).await?;
                    tracing::info!(alias, expression, "scanned import");
                    sandbox.absorb(grant);
                    scans.push(Step::Scan { alias: alias.clone(), expression });
                }
                Import::Attach { alias, source, database } => {
                    let config = state.resolve(source, database.as_deref()).await?;
                    attachments.push(attach::plan(alias, &config)?);
                }
                Import::Query { .. } | Import::Excel { .. } => {}
            }
        }

        let sql = substitute(&program.sql, root.as_ref())?;
        let spill = Spill::new()?;
        let (steps, mut instructions) = tokio::sync::mpsc::channel::<Step>(2);
        let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

        // DuckDB is synchronous, so it gets its own thread rather than stalling
        // the runtime for the length of the query. It owns the connection for the
        // whole run now — the rows arrive while it is open, which is what lets an
        // import of any size through.
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            let connection = open_duckdb_with(Setup {
                sandbox: &sandbox,
                schema: None,
                extensions: &[],
                attachments: &attachments,
                spill: Some(&spill.0),
            })?;
            if build(&connection, &mut instructions)? {
                run(&connection, &sql, &sink)?;
            }
            // Held until the query is done: DuckDB may still be reading what it
            // spilled there.
            drop(spill);
            Ok(())
        });

        // Second pass: fill the session. Views first, so an import failing does not
        // leave a half-built one behind.
        let fed = feed(state, &program, root.as_ref(), &steps, scans, limits).await;
        if let Err(e) = fed {
            // Tell the worker not to answer from what did arrive, then let its own
            // error win if it had one — a closed channel is a symptom, not a cause.
            let _ = steps.send(Step::Stop).await;
            drop(steps);
            worker.await.map_err(|e| Error::Federated(format!("the federated query panicked: {e}")))??;
            Err(e)?;
            return;
        }
        // Nothing more to send: the worker takes the closed channel as "go".
        drop(steps);

        while let Some(batch) = source.recv().await {
            yield batch?;
        }
        worker
            .await
            .map_err(|e| Error::Federated(format!("the federated query panicked: {e}")))??;
    })
}

/// Send the session everything it needs, in the order it needs it.
async fn feed(
    state: &AppState,
    program: &Program,
    root: Option<&PathBuf>,
    steps: &tokio::sync::mpsc::Sender<Step>,
    scans: Vec<Step>,
    limits: Limits,
) -> Result<()> {
    for scan in scans {
        send(steps, scan).await?;
    }

    for import in &program.imports {
        match import {
            Import::Query { alias, source, database, sql } => {
                stream_query(
                    state,
                    steps,
                    alias,
                    source,
                    database.as_deref(),
                    sql,
                    limits.max_import_rows,
                )
                .await?;
            }
            Import::Excel { alias, path, sheet } => {
                // Calamine reads a whole sheet before it can name a column, so
                // there is nothing here to stream.
                let table = materialise_excel(alias, root, path, sheet.as_deref())?;
                tracing::info!(alias = %table.alias, rows = table.rows.len(), "read a spreadsheet");
                send(steps, Step::Begin { alias: table.alias.clone(), columns: table.columns }).await?;
                send(steps, Step::Rows(table.rows)).await?;
                send(steps, Step::Seal).await?;
            }
            Import::Files { .. } | Import::Attach { .. } => {}
        }
    }
    Ok(())
}
