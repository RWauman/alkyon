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
struct Materialised {
    alias: String,
    columns: Vec<ColumnMeta>,
    /// Every cell as text; DuckDB does the casting.
    rows: Vec<Vec<Option<String>>>,
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_literal(text: &str) -> String {
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

/// Open an in-memory DuckDB, confine it, then freeze the configuration.
///
/// This matters more than it looks: DuckDB can read and write arbitrary files and
/// load extensions, so an unconfined instance behind an HTTP endpoint is arbitrary
/// file access and arbitrary code loading. `lock_configuration` is what stops a
/// user query from undoing any of it.
fn open_duckdb(roots: &[PathBuf]) -> Result<Connection> {
    let connection = Connection::open_in_memory().map_err(federated)?;

    // Before anything is locked down, and best effort: the `parquet` and `json`
    // Cargo features link these in, in which case they may already be registered
    // and `LOAD` is redundant. What proves they work is a query, not this call.
    for extension in STATIC_EXTENSIONS {
        if let Err(e) = connection.execute_batch(&format!("LOAD {extension};")) {
            tracing::debug!(extension, error = %e, "extension not loaded explicitly");
        }
    }

    let mut setup = String::new();
    if !roots.is_empty() {
        let list = roots
            .iter()
            .map(|root| quote_literal(&root.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        setup.push_str(&format!("SET allowed_directories = [{list}];\n"));
        // No `file_search_path`: with external access disabled, the permission
        // check happens on the path as written, before any search path applies, so
        // a relative one is refused however the search path is set. Files are
        // addressed through the `${folder}` placeholder instead — one rule for
        // reads and writes alike. See [`substitute`].
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
fn load(connection: &Connection, table: &Materialised) -> Result<()> {
    let staging = format!("{}__alkyon_raw", table.alias);
    let placeholders = (0..table.columns.len())
        .map(|index| format!("c{index} VARCHAR"))
        .collect::<Vec<_>>()
        .join(", ");

    connection
        .execute_batch(&format!(
            "CREATE TABLE {} ({placeholders});",
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
            "CREATE TABLE {alias} AS SELECT {projection} FROM {staging};\nDROP TABLE {staging};",
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
fn run(
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
                .map(|(index, name)| ColumnMeta {
                    name: name.clone(),
                    type_name: statement.column_type(index).to_string().to_lowercase(),
                    logical: LogicalType::Unknown,
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
        // Decimals, dates, intervals, lists, structs: rendered by DuckDB itself so
        // the exact digits survive, exactly as `numeric` does on the native path.
        Ok(other) => match duckdb::types::Value::from(other) {
            duckdb::types::Value::Null => Value::Null,
            value => Value::String(format!("{value:?}")),
        },
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
        let roots: Vec<PathBuf> = root.clone().into_iter().collect();

        // Everything the sources have to give, gathered before DuckDB opens.
        let mut tables = Vec::new();
        for import in &program.imports {
            let table = match import {
                Import::Query { alias, source, database, sql } => {
                    materialise_query(
                        state,
                        alias,
                        source,
                        database.as_deref(),
                        sql,
                        limits.max_import_rows,
                    )
                    .await?
                }
                Import::Excel { alias, path, sheet } => {
                    materialise_excel(alias, root.as_ref(), path, sheet.as_deref())?
                }
            };
            tracing::info!(alias = %table.alias, rows = table.rows.len(), "materialised import");
            tables.push(table);
        }

        let sql = substitute(&program.sql, root.as_ref())?;
        let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

        // DuckDB is synchronous, so it gets its own thread rather than stalling
        // the runtime for the length of the query.
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            let connection = open_duckdb(&roots)?;
            for table in &tables {
                load(&connection, table)?;
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
