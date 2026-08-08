//! DuckDB as a federator: materialise each import, then run the buffer over them.
//!
//! Imports rather than `ATTACH`, for now. It keeps the promise the whole project
//! rests on — *pure dialect per source*: the `@import` line is written in the
//! source's own SQL and nothing rewrites it, whereas `ATTACH` would have DuckDB's
//! planner generate the remote query. It also works offline and with every
//! connector, present and future. The cost is real and worth stating: the rows
//! travel through Alkyon, there is no predicate pushdown, and so there is a cap.

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
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How many rows a single import may pull. Exceeding it is an error, not a
    /// silent truncation: a join quietly missing half its rows is worse than a
    /// query that failed.
    pub max_import_rows: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_import_rows: 1_000_000,
        }
    }
}

impl Limits {
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        if let Some(rows) = std::env::var("ALKYON_IMPORT_MAX_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            limits.max_import_rows = rows;
        }
        limits
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
/// Anything absent here stays unavailable on purpose: `delta` and `iceberg` would
/// have to be downloaded at runtime, and community extensions are refused
/// outright.
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

/// An import, once it is ready for the DuckDB session.
enum Prepared {
    /// Rows pulled through Alkyon and rebuilt as a table.
    Rows(Materialised),
    /// A path DuckDB reads for itself.
    ///
    /// Worth the separate arm: a materialised import turns every cell into a
    /// `String` in this process, which for a 2.5M-row parquet is minutes and
    /// gigabytes. A scan is a view over the file, so DuckDB reads the columns it
    /// needs and nothing crosses the process at all.
    Scan {
        alias: String,
        /// e.g. `read_parquet('C:/data/2022/*.parquet')`
        expression: String,
        /// What the session must be allowed to touch for that to bind.
        grant: Sandbox,
    },
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

/// Pull an import's rows from its source. Async, because this is ordinary
/// connector work — the DuckDB half happens later, on a blocking thread.
async fn materialise_query(
    state: &AppState,
    alias: &str,
    source: &str,
    database: Option<&str>,
    sql: &str,
    cap: usize,
) -> Result<Materialised> {
    let connection = state.open(source, database).await?;
    let mut batches = connection.execute(sql);

    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();

    while let Some(batch) = batches.next().await {
        match batch? {
            RowBatch::Columns(meta) => {
                if columns.is_empty() {
                    columns = meta.as_ref().clone();
                } else {
                    // A batch of several statements has no single shape to import.
                    return Err(Error::BadRequest(format!(
                        "@import {alias}: the SQL returned more than one result set"
                    )));
                }
            }
            RowBatch::Rows(batch) => {
                if rows.len() + batch.len() > cap {
                    return Err(Error::BadRequest(format!(
                        "@import {alias}: more than {cap} rows. Narrow the import, or raise \
                         ALKYON_IMPORT_MAX_ROWS."
                    )));
                }
                rows.extend(
                    batch
                        .iter()
                        .map(|row| row.iter().map(as_text).collect::<Vec<_>>()),
                );
            }
            RowBatch::Affected(_) => {}
        }
    }

    if columns.is_empty() {
        return Err(Error::BadRequest(format!(
            "@import {alias}: the SQL returned no result set"
        )));
    }

    Ok(Materialised {
        alias: alias.to_owned(),
        columns,
        rows,
    })
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
) -> Result<Prepared> {
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

    Ok(Prepared::Scan {
        alias: alias.to_owned(),
        expression: format!("{reader}({})", quote_literal(&target)),
        // A folder source grants its folder — a glob has to be listed before it
        // can be opened. A file source grants only that file, so importing it
        // here is no wider than querying it directly.
        grant: if root.is_dir() {
            Sandbox::directory(root)
        } else {
            Sandbox::file(root)
        },
    })
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

/// Install and load an extension that is not linked in.
///
/// `LOAD` first: after the first time there is nothing to fetch. `INSTALL`
/// reaches `extensions.duckdb.org` and writes to DuckDB's own extension
/// directory, which is why it happens for a source that cannot be read without it
/// and never as a side effect of anything else.
fn fetch_extension(connection: &Connection, name: &str) -> Result<()> {
    if connection.execute_batch(&format!("LOAD {name};")).is_ok() {
        return Ok(());
    }
    tracing::info!(extension = name, "installing a DuckDB extension");
    connection
        .execute_batch(&format!("INSTALL {name}; LOAD {name};"))
        .map_err(|e| {
            Error::Federated(format!(
                "cannot install DuckDB's `{name}` extension, which is what reads this source: {e}"
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

    fetch_extension(&connection, "azure")?;
    for extension in extensions {
        fetch_extension(&connection, extension)?;
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
    let connection = Connection::open_in_memory().map_err(federated)?;
    for extension in extensions {
        fetch_extension(&connection, extension)?;
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

/// Create `alias` in DuckDB from already-pulled rows.
///
/// Two steps on purpose: an all-VARCHAR staging table, then a `CAST` into the real
/// one. DuckDB parses the text, so decimals keep their digits and dates parse
/// without a hand-written converter per dialect.
pub(crate) fn load(connection: &Connection, table: &Materialised) -> Result<()> {
    let staging = format!("{}__alkyon_raw", table.alias);
    let placeholders = (0..table.columns.len())
        .map(|index| format!("c{index} VARCHAR"))
        .collect::<Vec<_>>()
        .join(", ");

    // Explicitly in `main`: the appender resolves a bare table name there whatever
    // the search path says, and a folder source sets the search path to `public`.
    connection
        .execute_batch(&format!(
            "CREATE TABLE main.{} ({placeholders});",
            quote_identifier(&staging)
        ))
        .map_err(federated)?;

    {
        let mut appender = connection.appender(&staging).map_err(federated)?;
        for row in &table.rows {
            // `&dyn ToSql` over Option<String> gives NULL for None.
            let values: Vec<&dyn duckdb::ToSql> =
                row.iter().map(|cell| cell as &dyn duckdb::ToSql).collect();
            appender.append_row(&values[..]).map_err(federated)?;
        }
        appender.flush().map_err(federated)?;
    }

    let projection = table
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
            alias = quote_identifier(&table.alias),
            staging = quote_identifier(&staging),
        ))
        .map_err(|e| {
            Error::Federated(format!(
                "@import {}: could not type the imported rows: {e}",
                table.alias
            ))
        })?;
    Ok(())
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
        // Decimals, dates, intervals, lists, structs: rendered by DuckDB itself so
        // the exact digits survive, exactly as `numeric` does on the native path.
        Ok(other) => match duckdb::types::Value::from(other) {
            duckdb::types::Value::Null => Value::Null,
            value => Value::String(format!("{value:?}")),
        },
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

        // Everything the sources have to give, gathered before DuckDB opens.
        let mut tables = Vec::new();
        for import in &program.imports {
            let prepared = match import {
                Import::Query { alias, source, database, sql } => {
                    Prepared::Rows(materialise_query(
                        state,
                        alias,
                        source,
                        database.as_deref(),
                        sql,
                        limits.max_import_rows,
                    )
                    .await?)
                }
                Import::Files { alias, source, pattern } => {
                    prepare_scan(state, alias, source, pattern).await?
                }
                Import::Excel { alias, path, sheet } => {
                    Prepared::Rows(materialise_excel(alias, root.as_ref(), path, sheet.as_deref())?)
                }
            };
            match &prepared {
                Prepared::Rows(table) => {
                    tracing::info!(alias = %table.alias, rows = table.rows.len(), "materialised import")
                }
                Prepared::Scan { alias, expression, grant } => {
                    tracing::info!(alias, expression, "scanned import");
                    sandbox.absorb(grant.clone());
                }
            }
            tables.push(prepared);
        }

        let sql = substitute(&program.sql, root.as_ref())?;
        let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

        // DuckDB is synchronous, so it gets its own thread rather than stalling
        // the runtime for the length of the query.
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            let connection = open_duckdb(&sandbox)?;
            for table in &tables {
                match table {
                    Prepared::Rows(table) => load(&connection, table)?,
                    Prepared::Scan { alias, expression, .. } => {
                        // A view, not a table: binding it reads the file's header
                        // and nothing else until the query asks for rows.
                        connection
                            .execute_batch(&format!(
                                "CREATE VIEW {} AS SELECT * FROM {expression};",
                                quote_identifier(alias)
                            ))
                            .map_err(|e| {
                                Error::Federated(format!("@import {alias}: {e}"))
                            })?;
                    }
                }
            }
            run(&connection, &sql, &sink)
        });

        while let Some(batch) = source.recv().await {
            yield batch?;
        }
        worker
            .await
            .map_err(|e| Error::Federated(format!("the federated query panicked: {e}")))??;
    })
}
