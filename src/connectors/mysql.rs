//! MySQL and MariaDB, over sqlx's pure-Rust protocol implementation.
//!
//! Close to [`super::postgres`] on purpose — same pool cache, same credential
//! fingerprint, same streaming shape — with one structural difference: **MySQL
//! has no schema layer**. A schema *is* a database, so `information_schema`
//! reports `table_schema` where PostgreSQL would report a namespace inside a
//! database. Tables therefore come back with their database as their schema,
//! which is also exactly how you qualify one in SQL: `` `db`.`table` ``.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::{Column, Either, Executor, MySqlPool, Row, TypeInfo, ValueRef};
use tokio::sync::Mutex;

use super::{Connection, Connector};
use crate::error::{Error, Result};
use crate::model::{
    AuthConfig, ColumnInfo, ColumnMeta, Dialect, LogicalType, RowBatch, SourceConfig, TableInfo,
    TableKind, TableSchema, TlsMode, BATCH_ROWS,
};

/// Everything that, if changed, must not reuse an existing pool. Same reasoning
/// as the PostgreSQL one: fold the credential in so a wrong password can never
/// be answered by a pool a right one built.
fn fingerprint(cfg: &SourceConfig, db: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    cfg.host.hash(&mut hasher);
    cfg.port().hash(&mut hasher);
    db.hash(&mut hasher);
    cfg.tls.hash(&mut hasher);
    match &cfg.auth {
        AuthConfig::Password { username, password } => {
            "password".hash(&mut hasher);
            username.hash(&mut hasher);
            password.hash(&mut hasher);
        }
        other => other.method().hash(&mut hasher),
    }
    hasher.finish()
}

struct Cached {
    fingerprint: u64,
    pool: MySqlPool,
}

/// MySQL *can* reach another database on the same connection, but the default
/// database decides how unqualified names in the editor resolve — so pools stay
/// keyed by `(source id, database)` as they are for PostgreSQL.
#[derive(Default)]
struct PoolCache(Mutex<HashMap<(String, String), Cached>>);

impl PoolCache {
    async fn get(&self, cfg: &SourceConfig, db: &str) -> Result<MySqlPool> {
        let key = (cfg.id.clone(), db.to_owned());
        let fingerprint = fingerprint(cfg, db);
        let mut cache = self.0.lock().await;

        if let Some(cached) = cache.get(&key) {
            if cached.fingerprint == fingerprint {
                return Ok(cached.pool.clone());
            }
            cache.remove(&key);
        }

        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options(cfg, db)?)
            .await?;
        cache.insert(
            key,
            Cached {
                fingerprint,
                pool: pool.clone(),
            },
        );
        Ok(pool)
    }
}

fn options(cfg: &SourceConfig, db: &str) -> Result<MySqlConnectOptions> {
    let AuthConfig::Password { username, password } = &cfg.auth else {
        return Err(Error::Unsupported(format!(
            "MySQL sources need password authentication, got `{}`",
            cfg.auth.method()
        )));
    };
    Ok(MySqlConnectOptions::new()
        .host(&cfg.host)
        .port(cfg.port())
        .username(username)
        .password(password)
        .database(db)
        .ssl_mode(match cfg.tls {
            TlsMode::Disable => MySqlSslMode::Disabled,
            TlsMode::Prefer => MySqlSslMode::Preferred,
            TlsMode::Require => MySqlSslMode::VerifyIdentity,
            TlsMode::TrustCertificate => MySqlSslMode::Required,
        }))
}

#[derive(Default)]
pub struct MySqlConnector {
    pools: Arc<PoolCache>,
}

#[async_trait]
impl Connector for MySqlConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let pool = self.pools.get(config, config.database()).await?;
        // A pool built earlier proves nothing about the server *now*.
        pool.acquire().await?;
        Ok(Box::new(MySqlConnection {
            cfg: config.clone(),
            pools: Arc::clone(&self.pools),
        }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }
}

pub struct MySqlConnection {
    cfg: SourceConfig,
    pools: Arc<PoolCache>,
}

/// `performance_schema` and `sys` are instrumentation, never user data, and they
/// would otherwise be the loudest thing in the tree on a fresh server.
/// `information_schema` stays: it is this connector's default database.
const LIST_DATABASES: &str = "\
SELECT CAST(schema_name AS CHAR) AS name
FROM information_schema.schemata
WHERE schema_name NOT IN ('performance_schema', 'sys')
ORDER BY schema_name";

/// Every column read out of `information_schema` is cast explicitly.
///
/// Its declared types drift between MySQL 5.7, MySQL 8 and MariaDB — an ordinal
/// is `bigint unsigned` in one and `int unsigned` in another, and sqlx decodes
/// strictly by the type the server reports. Casting pins the shape so the
/// connector does not depend on which server it happens to be talking to.
const LIST_TABLES: &str = "\
SELECT CAST(table_name AS CHAR) AS name,
       CAST(table_type AS CHAR) AS kind
FROM information_schema.tables
WHERE table_schema = ?
ORDER BY table_name";

const LIST_COLUMNS: &str = "\
SELECT CAST(column_name AS CHAR)      AS name,
       CAST(ordinal_position AS SIGNED) AS ordinal,
       CAST(column_type AS CHAR)      AS data_type,
       CAST(is_nullable AS CHAR)      AS nullable,
       CAST(column_default AS CHAR)   AS default_expr,
       CAST(column_key AS CHAR)       AS column_key
FROM information_schema.columns
WHERE table_schema = ? AND table_name = ?
ORDER BY ordinal_position";

/// `LIST_COLUMNS` widened to a whole database, so a snapshot is one round trip
/// rather than one per table.
const SNAPSHOT: &str = "\
SELECT CAST(c.table_name AS CHAR)         AS table_name,
       CAST(t.table_type AS CHAR)         AS kind,
       CAST(c.column_name AS CHAR)        AS name,
       CAST(c.ordinal_position AS SIGNED) AS ordinal,
       CAST(c.column_type AS CHAR)        AS data_type,
       CAST(c.is_nullable AS CHAR)        AS nullable,
       CAST(c.column_default AS CHAR)     AS default_expr,
       CAST(c.column_key AS CHAR)         AS column_key
FROM information_schema.columns c
JOIN information_schema.tables t
  ON t.table_schema = c.table_schema AND t.table_name = c.table_name
WHERE c.table_schema = ?
ORDER BY c.table_name, c.ordinal_position";

fn table_kind(kind: &str) -> TableKind {
    match kind {
        "VIEW" | "SYSTEM VIEW" => TableKind::View,
        _ => TableKind::Table,
    }
}

fn column_info(row: &MySqlRow) -> ColumnInfo {
    ColumnInfo {
        name: row.get("name"),
        ordinal: row.get::<i64, _>("ordinal") as i32,
        data_type: row.get("data_type"),
        nullable: row.get::<String, _>("nullable") == "YES",
        is_primary_key: row.get::<String, _>("column_key") == "PRI",
        default: row.get("default_expr"),
    }
}

impl MySqlConnection {
    /// MySQL has no schema below a database, so an empty schema means "whichever
    /// database this call is about" — see [`crate::model::SourceKind`].
    fn schema_of<'a>(db: &'a str, schema: &'a str) -> &'a str {
        if schema.is_empty() {
            db
        } else {
            schema
        }
    }
}

#[async_trait]
impl Connection for MySqlConnection {
    async fn list_databases(&self) -> Result<Vec<String>> {
        let pool = self.pools.get(&self.cfg, self.cfg.database()).await?;
        let rows = sqlx::query(LIST_DATABASES).fetch_all(&pool).await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
    }

    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(LIST_TABLES).bind(db).fetch_all(&pool).await?;
        Ok(rows
            .iter()
            .map(|r| TableInfo {
                // The database is the schema. Reporting it keeps the tree and the
                // qualified name the explorer inserts both correct.
                schema: db.to_owned(),
                name: r.get("name"),
                kind: table_kind(&r.get::<String, _>("kind")),
            })
            .collect())
    }

    async fn list_columns(&self, db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(LIST_COLUMNS)
            .bind(Self::schema_of(db, schema))
            .bind(table)
            .fetch_all(&pool)
            .await?;
        Ok(rows.iter().map(column_info).collect())
    }

    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(SNAPSHOT).bind(db).fetch_all(&pool).await?;

        // Ordered by table, so a running "current table" is enough.
        let mut tables: Vec<TableSchema> = Vec::new();
        for row in &rows {
            let name: String = row.get("table_name");
            if tables.last().is_none_or(|last| last.name != name) {
                tables.push(TableSchema {
                    schema: db.to_owned(),
                    name,
                    kind: table_kind(&row.get::<String, _>("kind")),
                    columns: Vec::new(),
                });
            }
            tables.last_mut().unwrap().columns.push(column_info(row));
        }
        Ok(tables)
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let pool = self.pools.get(&self.cfg, self.cfg.database()).await?;
            // The text protocol, so a whole editor buffer of statements runs as
            // one batch. sqlx negotiates CLIENT_MULTI_STATEMENTS at handshake,
            // which is what makes that work.
            let mut results = pool.fetch_many(sqlx::raw_sql(sql));
            let mut columns: Option<Arc<Vec<ColumnMeta>>> = None;
            let mut buffered: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);

            while let Some(item) = results.next().await {
                match item? {
                    // End of one statement in the batch.
                    Either::Left(done) => {
                        if !buffered.is_empty() {
                            yield RowBatch::Rows(std::mem::take(&mut buffered));
                        }
                        if columns.is_none() {
                            yield RowBatch::Affected(done.rows_affected());
                        }
                        columns = None;
                    }
                    Either::Right(row) => {
                        if columns.is_none() {
                            let meta = Arc::new(result_columns(&row));
                            columns = Some(Arc::clone(&meta));
                            yield RowBatch::Columns(meta);
                        }
                        buffered.push(row_values(&row));
                        if buffered.len() >= BATCH_ROWS {
                            yield RowBatch::Rows(std::mem::take(&mut buffered));
                        }
                    }
                }
            }
            if !buffered.is_empty() {
                yield RowBatch::Rows(buffered);
            }
        })
    }
}

fn result_columns(row: &MySqlRow) -> Vec<ColumnMeta> {
    row.columns()
        .iter()
        .map(|c| {
            let type_name = c.type_info().name().to_owned();
            ColumnMeta {
                logical: logical_type(&type_name),
                name: c.name().to_owned(),
                type_name,
            }
        })
        .collect()
}

/// Mirrors the arms of [`value_at`]: whatever that renders, this names.
fn logical_type(type_name: &str) -> LogicalType {
    match type_name {
        "BOOLEAN" => LogicalType::Bool,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" | "TINYINT UNSIGNED"
        | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED" | "BIGINT UNSIGNED"
        | "YEAR" | "BIT" => LogicalType::Int,
        "FLOAT" | "DOUBLE" => LogicalType::Float,
        "DECIMAL" => LogicalType::Decimal,
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            LogicalType::Text
        }
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        // Both, deliberately: see [`value_at`].
        "DATETIME" | "TIMESTAMP" => LogicalType::Timestamp,
        "JSON" => LogicalType::Json,
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            LogicalType::Binary
        }
        _ => LogicalType::Unknown,
    }
}

fn row_values(row: &MySqlRow) -> Vec<Value> {
    (0..row.len()).map(|i| value_at(row, i)).collect()
}

/// Decode one cell into JSON, dispatching on the type sqlx reported for it.
///
/// `decimal` travels as a string for the same reason it does on the PostgreSQL
/// side: `serde_json` numbers cannot hold arbitrary precision, and rounding a
/// money column through `f64` is not a workbench's decision to make.
fn value_at(row: &MySqlRow, i: usize) -> Value {
    macro_rules! json {
        ($t:ty) => {
            match row.try_get::<Option<$t>, _>(i) {
                Ok(Some(v)) => serde_json::to_value(v).unwrap_or(Value::Null),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        };
    }
    macro_rules! text {
        ($t:ty) => {
            match row.try_get::<Option<$t>, _>(i) {
                Ok(Some(v)) => Value::String(v.to_string()),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        };
    }

    match row.try_get_raw(i) {
        Ok(raw) if raw.is_null() => return Value::Null,
        Err(e) => return Value::String(format!("<decode error: {e}>")),
        _ => {}
    }

    match row.column(i).type_info().name() {
        // MySQL has no boolean: this is `tinyint(1)`, which sqlx names BOOLEAN.
        "BOOLEAN" => json!(bool),
        "TINYINT" => json!(i8),
        "SMALLINT" => json!(i16),
        "MEDIUMINT" | "INT" => json!(i32),
        "BIGINT" => json!(i64),
        "TINYINT UNSIGNED" => json!(u8),
        "SMALLINT UNSIGNED" => json!(u16),
        // Both fit in 32 bits unsigned; MEDIUMINT tops out at 16.7M.
        "MEDIUMINT UNSIGNED" | "INT UNSIGNED" => json!(u32),
        "BIGINT UNSIGNED" => json!(u64),
        // sqlx only decodes these when the server sets the UNSIGNED flag, which
        // it does for both — a server that does not gives a visible decode error
        // in the cell rather than a wrong value.
        "YEAR" => json!(u16),
        "BIT" => json!(u64),
        "FLOAT" => json!(f32),
        "DOUBLE" => json!(f64),
        "DECIMAL" => text!(rust_decimal::Decimal),
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            json!(String)
        }
        "DATE" => json!(chrono::NaiveDate),
        "TIME" => json!(chrono::NaiveTime),
        // Naive for both, on purpose. A MySQL `timestamp` is returned in the
        // *session* time zone, so stamping it UTC would be a confident lie; what
        // is shown is the wall clock the server sent.
        "DATETIME" | "TIMESTAMP" => json!(chrono::NaiveDateTime),
        "JSON" => json!(Value),
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            match row.try_get::<Option<Vec<u8>>, _>(i) {
                Ok(Some(v)) => Value::String(hex(&v)),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        }
        // `geometry` and anything a future server adds. Cast it in the query.
        other => Value::String(format!("<unsupported type {other}>")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
