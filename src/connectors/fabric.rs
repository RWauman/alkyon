//! SQL Server reached through **DuckDB's `mssql` extension** instead of
//! `tiberius` — and still answering **T-SQL**.
//!
//! It exists for one endpoint alkyon cannot otherwise reach. A Fabric SQL
//! analytics endpoint accepts the first login and answers with a routing token
//! naming the node that holds the warehouse; `tiberius` cannot send the two names
//! that node wants, so the ordinary [`SourceKind::MsSql`] source stops there. This
//! path never involves `tiberius`: the extension speaks its own TDS and takes the
//! same Entra sign-in, already turned into a bearer token by the time a connector
//! sees it.
//!
//! **The dialect does not change.** The extension's `mssql_scan` takes a query and
//! runs it on the server verbatim, so what alkyon sends is:
//!
//! ```sql
//! SELECT * FROM mssql_scan('server', 'SELECT TOP 10 * FROM sales.customer')
//! ```
//!
//! `top`, `sys.*`, window functions, `@@VERSION` — all arrive as written, and the
//! explorer's metadata is the same T-SQL the native connector uses, so the two
//! describe a server identically. DuckDB is a pipe here, not a query engine.
//!
//! What it costs, measured rather than assumed:
//!
//! - **It reads.** `mssql_scan` binds a result set, so a statement returning no
//!   columns cannot go through it, and the attachment is `READ_ONLY` besides. For a
//!   SQL analytics endpoint, which is read-only anyway, that is no loss.
//! - **One result set** per run. A batch of several statements yields the first.
//! - **About a second to open**, spent in the extension's catalogue round trip at
//!   `ATTACH` time. Every query pays it, since a connection is built per query.
//! - **It is third-party code** — see [`crate::federation::attach::community_allowed`].

use std::sync::{Arc, Mutex};

use async_stream::try_stream;
use async_trait::async_trait;
use duckdb::Connection as Duck;
use futures::stream::BoxStream;

use super::{Connection, Connector};
use crate::connectors::mssql;
use crate::error::{Error, Result};
use crate::federation::{self, attach, quote_literal};
use crate::model::{
    ColumnInfo, Dialect, RowBatch, SourceConfig, TableInfo, TableKind, TableSchema,
};

/// What the attached catalogue is called inside DuckDB.
///
/// Never seen by anyone: every query goes through `mssql_scan`, which takes this
/// name as its first argument. Nothing a user writes is resolved against it.
const ALIAS: &str = "alkyon_server";

pub struct FabricConnector;

#[async_trait]
impl Connector for FabricConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let config = config.clone();
        // Opening is a network round trip inside a synchronous library, so it goes
        // to a blocking thread rather than stalling the runtime.
        let session = tokio::task::spawn_blocking(move || open(&config))
            .await
            .map_err(|e| Error::Federated(format!("opening the session panicked: {e}")))??;
        Ok(Box::new(session))
    }

    /// T-SQL, because that is what the server receives. The editor stays in SQL
    /// Server mode and `[bracket]` quoting is right.
    fn dialect(&self) -> Dialect {
        Dialect::TSql
    }
}

fn open(config: &SourceConfig) -> Result<FabricConnection> {
    let plan = attach::plan_for("this source", ALIAS, config)?;
    let duck = Duck::open_in_memory().map_err(|e| Error::Federated(e.to_string()))?;
    federation::fetch_extension(&duck, plan.engine.extension(), plan.engine.repository())?;

    for statement in plan.statements() {
        duck.execute_batch(statement).map_err(|e| {
            // Never the statement: the credential is in it.
            Error::Remote(explain(&e.to_string(), config))
        })?;
    }

    // The same confinement a federated session gets, and for the same reason: this
    // DuckDB has no business reading the machine it runs on. Nothing a user writes
    // reaches its parser — their SQL travels inside a string literal to the server
    // — but defence in depth costs nothing here.
    duck.execute_batch(
        "SET enable_external_access = false;\n\
         SET allow_community_extensions = false;\n\
         SET allow_unsigned_extensions = false;\n\
         SET autoinstall_known_extensions = false;\n\
         SET autoload_known_extensions = false;\n\
         SET lock_configuration = true;\n",
    )
    .map_err(|e| Error::Federated(format!("could not secure the session: {e}")))?;

    let session = FabricConnection {
        duck: Arc::new(Mutex::new(duck)),
        database: config.database().to_owned(),
    };
    // Prove the credentials before the source is registered, the way every other
    // connector does — building the attachment alone proves less than it looks.
    session.rows("SELECT 1 AS ok", |_| Ok(()))?;
    Ok(session)
}

/// Turn what the extension says into something worth reading.
fn explain(said: &str, config: &SourceConfig) -> String {
    if said.contains("Login failed") || said.contains("18456") {
        return format!(
            "the server refused the login for `{}`. If this is a Fabric endpoint, check the \
             sign-in is the account with access to the workspace.",
            config.host
        );
    }
    if said.contains("Cannot resolve hostname") {
        return format!("`{}` did not resolve — check the endpoint name.", config.host);
    }
    said.to_owned()
}

pub struct FabricConnection {
    /// DuckDB is synchronous and `!Sync`; the mutex is what lets one connection be
    /// shared by the trait's `&self` methods. Queries against one source therefore
    /// serialise, which is the right trade for a workbench holding one catalogue.
    duck: Arc<Mutex<Duck>>,
    /// The database the attachment is bound to, and the one a bare name means.
    database: String,
}

/// Wrap a T-SQL statement in the call that sends it to the server verbatim.
fn scan(sql: &str) -> String {
    format!(
        "SELECT * FROM mssql_scan({}, {})",
        quote_literal(ALIAS),
        quote_literal(sql)
    )
}

/// A T-SQL statement run in `db` rather than the attached database.
///
/// Three-part names would do it for `sys.*`, but not for a query someone typed, and
/// one rule is better than two. `USE` is a batch of its own; the extension returns
/// the last result set, which is the one that matters.
fn in_database(db: &str, sql: &str) -> String {
    if db.is_empty() {
        return sql.to_owned();
    }
    format!("USE [{}]; {sql}", db.replace(']', "]]"))
}

impl FabricConnection {
    /// Run a query on the server and map each row.
    fn rows<T>(&self, sql: &str, map: impl Fn(&duckdb::Row<'_>) -> Result<T>) -> Result<Vec<T>> {
        let duck = self.duck.lock().map_err(poisoned)?;
        let wrapped = scan(sql);
        let mut statement = duck.prepare(&wrapped).map_err(|e| remote(&e.to_string()))?;
        let mut query = statement.query([]).map_err(|e| remote(&e.to_string()))?;

        let mut out = Vec::new();
        while let Some(row) = query.next().map_err(|e| remote(&e.to_string()))? {
            out.push(map(row)?);
        }
        Ok(out)
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::Federated("the session was left in a broken state by an earlier failure".into())
}

/// The extension's errors, cleaned of the DuckDB wrapping around them.
fn remote(said: &str) -> Error {
    // `mssql_scan` binds a result set, so a statement with nothing to return fails
    // here rather than on the server. Saying what the source is for beats repeating
    // DuckDB's binder.
    if said.contains("must return at least one column") {
        return Error::Remote(
            "this source reads: the statement returned no rows, and the connection is \
             read-only. Use an ordinary SQL Server source to write."
                .into(),
        );
    }
    let trimmed = said
        .split_once("MSSQL Error: ")
        .map_or(said, |(_, rest)| rest);
    Error::Remote(trimmed.lines().next().unwrap_or(said).to_owned())
}

/// Read a column that SQL Server may have sent as any width of integer.
fn as_i64(row: &duckdb::Row<'_>, index: usize) -> i64 {
    row.get::<_, i64>(index)
        .or_else(|_| row.get::<_, i32>(index).map(i64::from))
        .or_else(|_| row.get::<_, i16>(index).map(i64::from))
        .or_else(|_| row.get::<_, u8>(index).map(i64::from))
        .unwrap_or_default()
}

fn as_bool(row: &duckdb::Row<'_>, index: usize) -> bool {
    row.get::<_, bool>(index).unwrap_or(as_i64(row, index) != 0)
}

fn as_text(row: &duckdb::Row<'_>, index: usize) -> String {
    row.get::<_, String>(index).unwrap_or_default()
}

fn as_option(row: &duckdb::Row<'_>, index: usize) -> Option<String> {
    row.get::<_, Option<String>>(index).unwrap_or_default()
}

/// One row of `LIST_COLUMNS` or `SNAPSHOT`, which the native connector wrote and
/// this one reuses so the two describe a server identically.
fn column_at(row: &duckdb::Row<'_>, base: usize) -> ColumnInfo {
    ColumnInfo {
        name: as_text(row, base),
        ordinal: as_i64(row, base + 1) as i32,
        data_type: mssql::spell_type(
            &as_text(row, base + 2),
            as_i64(row, base + 3) as i16,
            as_i64(row, base + 4) as u8,
            as_i64(row, base + 5) as u8,
        ),
        nullable: as_bool(row, base + 6),
        default: as_option(row, base + 7),
        is_primary_key: as_bool(row, base + 8),
    }
}

#[async_trait]
impl Connection for FabricConnection {
    async fn list_databases(&self) -> Result<Vec<String>> {
        self.rows(mssql::LIST_DATABASES, |row| Ok(as_text(row, 0)))
    }

    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>> {
        self.rows(&in_database(db, mssql::LIST_TABLES), |row| {
            Ok(TableInfo {
                schema: as_text(row, 0),
                name: as_text(row, 1),
                kind: match as_text(row, 2).trim() {
                    "V" => TableKind::View,
                    _ => TableKind::Table,
                },
            })
        })
    }

    async fn list_columns(&self, db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        // The native connector parameterises this with @P1/@P2. There are no
        // parameters through `mssql_scan` — the query is one string — so the two
        // names become literals, quoted the way T-SQL wants.
        let sql = mssql::LIST_COLUMNS
            .replace("@P1", &tsql_literal(schema))
            .replace("@P2", &tsql_literal(table));
        self.rows(&in_database(db, &sql), |row| Ok(column_at(row, 0)))
    }

    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>> {
        let rows = self.rows(&in_database(db, mssql::SNAPSHOT), |row| {
            Ok((
                as_text(row, 0),
                as_text(row, 1),
                as_text(row, 2),
                column_at(row, 3),
            ))
        })?;

        // Ordered by schema, table, ordinal — so a change of the first two starts a
        // new table and nothing has to be looked up.
        let mut tables: Vec<TableSchema> = Vec::new();
        for (schema, name, kind, column) in rows {
            match tables.last_mut() {
                Some(last) if last.schema == schema && last.name == name => {
                    last.columns.push(column)
                }
                _ => tables.push(TableSchema {
                    schema,
                    name,
                    kind: match kind.trim() {
                        "V" => TableKind::View,
                        _ => TableKind::Table,
                    },
                    columns: vec![column],
                }),
            }
        }
        Ok(tables)
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let duck = Arc::clone(&self.duck);
            let wrapped = scan(&in_database(&self.database, sql));
            let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

            // DuckDB is synchronous, so it gets its own thread rather than stalling
            // the runtime for the length of the query.
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                let duck = duck.lock().map_err(poisoned)?;
                // Through the same cleaner the metadata calls use, or the failure
                // arrives wearing DuckDB's clothes: a missing table would report
                // `LINE 1: SELECT * FROM mssql_scan(…` and bury the server's own
                // sentence under the plumbing that carried it.
                federation::run(&duck, &wrapped, &sink).map_err(|e| remote(&e.to_string()))
            });

            while let Some(batch) = source.recv().await {
                yield batch?;
            }
            worker
                .await
                .map_err(|e| Error::Federated(format!("the query worker failed: {e}")))??;
        })
    }
}

/// A T-SQL string literal: single quotes doubled, and nothing else.
fn tsql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user's SQL travels inside a string literal, so a quote in it must not be
    /// able to end that literal — this is the only injection surface the design has.
    #[test]
    fn a_quote_in_the_query_cannot_end_the_literal() {
        // Written as exact text because the shape is the guarantee: every quote of
        // the user's is doubled, so the only `'` that closes the literal is the one
        // this function put there.
        assert_eq!(
            scan("SELECT * FROM t WHERE name = 'Ada'"),
            "SELECT * FROM mssql_scan('alkyon_server', 'SELECT * FROM t WHERE name = ''Ada''')"
        );

        // The shape that would end the call early if the escaping were missing —
        // and the reason a `contains` assertion is no use here: the payload is
        // present, in doubled form, which is precisely what makes it inert.
        assert_eq!(
            scan("x'); DROP TABLE t; --"),
            "SELECT * FROM mssql_scan('alkyon_server', 'x''); DROP TABLE t; --')"
        );

        // A quote already doubled by the user survives as four, so their own
        // literal still reads as one quote on the server.
        assert!(scan("name = 'O''Brien'").contains("''O''''Brien''"));
    }

    #[test]
    fn a_database_is_switched_by_a_batch_rather_than_a_prefix() {
        assert_eq!(
            in_database("warehouse", "SELECT 1"),
            "USE [warehouse]; SELECT 1"
        );
        // A bracket in a database name cannot end the identifier.
        assert_eq!(in_database("od]d", "SELECT 1"), "USE [od]]d]; SELECT 1");
        // Nothing to switch to.
        assert_eq!(in_database("", "SELECT 1"), "SELECT 1");
    }

    /// The binder's complaint is about a table function; the user's problem is that
    /// this source does not write. Say the second.
    #[test]
    fn a_statement_that_returns_nothing_says_what_the_source_is_for() {
        let error = remote("INTERNAL Error: Failed to bind \"mssql_scan\": Table function must return at least one column")
            .to_string();
        assert!(error.contains("this source reads"), "{error}");
        assert!(error.contains("read-only"), "{error}");
    }

    /// What the server said, not what DuckDB wrapped around it.
    #[test]
    fn the_servers_own_words_survive() {
        let error = remote("Invalid Input Error: MSSQL Error: Invalid object name 'sales.nope'.")
            .to_string();
        assert!(error.contains("Invalid object name 'sales.nope'."), "{error}");
        assert!(!error.contains("Invalid Input Error"), "{error}");
    }
}
