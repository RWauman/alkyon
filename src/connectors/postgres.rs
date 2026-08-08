use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{Column, Either, Executor, PgPool, Row, TypeInfo, ValueRef};
use tokio::sync::Mutex;

use super::{Connection, Connector};
use crate::error::{Error, Result};
use crate::model::{
    AuthConfig, ColumnInfo, ColumnMeta, Dialect, LogicalType, RowBatch, SourceConfig, TableInfo,
    TableKind, TableSchema, TlsMode, BATCH_ROWS,
};

/// Everything about a connection that, if changed, must not reuse an existing
/// pool — endpoint, identity and encryption. The credential is folded in as a
/// hash so a wrong password can never be answered by a pool a right one built.
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
        AuthConfig::AadToken { token } => {
            "aad".hash(&mut hasher);
            token.hash(&mut hasher);
        }
        // Refused further down as unsupported, but a pool keyed on an incomplete
        // identity is the kind of bug that only shows up as the wrong session.
        AuthConfig::Entra {
            tenant,
            client_id,
            refresh_token,
            ..
        } => {
            "entra".hash(&mut hasher);
            tenant.hash(&mut hasher);
            client_id.hash(&mut hasher);
            refresh_token.hash(&mut hasher);
        }
        // Neither carries a credential, so the method name is the whole identity.
        other @ (AuthConfig::Integrated | AuthConfig::None) => other.method().hash(&mut hasher),
    }
    hasher.finish()
}

struct Cached {
    fingerprint: u64,
    pool: PgPool,
}

/// PostgreSQL cannot query across databases on one connection, so pools are
/// keyed by `(source id, database)` and created on first use.
#[derive(Default)]
struct PoolCache(Mutex<HashMap<(String, String), Cached>>);

impl PoolCache {
    async fn get(&self, cfg: &SourceConfig, db: &str) -> Result<PgPool> {
        let key = (cfg.id.clone(), db.to_owned());
        let fingerprint = fingerprint(cfg, db);
        let mut cache = self.0.lock().await;

        if let Some(cached) = cache.get(&key) {
            if cached.fingerprint == fingerprint {
                return Ok(cached.pool.clone());
            }
            // Credentials or endpoint changed under the same id: the old pool
            // must not vouch for the new ones.
            cache.remove(&key);
        }

        let pool = PgPoolOptions::new()
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

fn options(cfg: &SourceConfig, db: &str) -> Result<PgConnectOptions> {
    let AuthConfig::Password { username, password } = &cfg.auth else {
        return Err(Error::Unsupported(format!(
            "PostgreSQL sources need password authentication, got `{}`",
            cfg.auth.method()
        )));
    };
    Ok(PgConnectOptions::new()
        .host(&cfg.host)
        .port(cfg.port())
        .username(username)
        .password(password)
        .database(db)
        .application_name("alkyon")
        .ssl_mode(match cfg.tls {
            TlsMode::Disable => PgSslMode::Disable,
            TlsMode::Prefer => PgSslMode::Prefer,
            TlsMode::Require => PgSslMode::VerifyFull,
            TlsMode::TrustCertificate => PgSslMode::Require,
        }))
}

#[derive(Default)]
pub struct PgConnector {
    pools: Arc<PoolCache>,
}

#[async_trait]
impl Connector for PgConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        // Building the pool connects eagerly, so a bad host or password fails here
        // rather than on the first query.
        let pool = self.pools.get(config, config.database()).await?;
        // A pool that was built earlier proves nothing about the server *now*, and
        // `connect` promises the source is reachable. Acquiring pings it.
        pool.acquire().await?;
        Ok(Box::new(PgConnection {
            cfg: config.clone(),
            pools: Arc::clone(&self.pools),
        }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::PgSql
    }
}

pub struct PgConnection {
    cfg: SourceConfig,
    pools: Arc<PoolCache>,
}

const LIST_DATABASES: &str = "\
SELECT datname
FROM pg_catalog.pg_database
WHERE NOT datistemplate
  AND has_database_privilege(datname, 'CONNECT')
ORDER BY datname";

const LIST_TABLES: &str = "\
SELECT n.nspname AS schema, c.relname AS name, c.relkind AS kind
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND n.nspname NOT LIKE 'pg_toast%'
ORDER BY n.nspname, c.relname";

const LIST_COLUMNS: &str = "\
SELECT a.attname                            AS name,
       a.attnum                             AS ordinal,
       format_type(a.atttypid, a.atttypmod) AS data_type,
       NOT a.attnotnull                     AS nullable,
       pg_get_expr(d.adbin, d.adrelid)      AS default_expr,
       COALESCE(i.indisprimary, false)      AS is_primary_key
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c        ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n    ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
LEFT JOIN pg_catalog.pg_index i   ON i.indrelid = a.attrelid
                                 AND i.indisprimary
                                 AND a.attnum = ANY (i.indkey)
WHERE n.nspname = $1
  AND c.relname = $2
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum";

/// `LIST_COLUMNS` widened to the whole database, carrying the relation kind so a
/// snapshot costs one round trip rather than one per table.
const SNAPSHOT: &str = "\
SELECT n.nspname                            AS schema,
       c.relname                            AS table_name,
       c.relkind                            AS kind,
       a.attname                            AS name,
       a.attnum                             AS ordinal,
       format_type(a.atttypid, a.atttypmod) AS data_type,
       NOT a.attnotnull                     AS nullable,
       pg_get_expr(d.adbin, d.adrelid)      AS default_expr,
       COALESCE(i.indisprimary, false)      AS is_primary_key
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c        ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n    ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
LEFT JOIN pg_catalog.pg_index i   ON i.indrelid = a.attrelid
                                 AND i.indisprimary
                                 AND a.attnum = ANY (i.indkey)
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND n.nspname NOT LIKE 'pg_toast%'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY n.nspname, c.relname, a.attnum";

#[async_trait]
impl Connection for PgConnection {
    async fn list_databases(&self) -> Result<Vec<String>> {
        let pool = self.pools.get(&self.cfg, self.cfg.database()).await?;
        let rows = sqlx::query(LIST_DATABASES).fetch_all(&pool).await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("datname")).collect())
    }

    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(LIST_TABLES).fetch_all(&pool).await?;
        Ok(rows
            .iter()
            .map(|r| TableInfo {
                schema: r.get("schema"),
                name: r.get("name"),
                // 'v' view, 'm' materialised view; everything else we asked for is table-like.
                kind: match r.get::<i8, _>("kind") as u8 {
                    b'v' | b'm' => TableKind::View,
                    _ => TableKind::Table,
                },
            })
            .collect())
    }

    async fn list_columns(&self, db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(LIST_COLUMNS)
            .bind(schema)
            .bind(table)
            .fetch_all(&pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: r.get("name"),
                ordinal: r.get::<i16, _>("ordinal") as i32,
                data_type: r.get("data_type"),
                nullable: r.get("nullable"),
                is_primary_key: r.get("is_primary_key"),
                default: r.get("default_expr"),
            })
            .collect())
    }

    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>> {
        let pool = self.pools.get(&self.cfg, db).await?;
        let rows = sqlx::query(SNAPSHOT).fetch_all(&pool).await?;

        // Rows arrive grouped by relation, so a running "current table" is enough.
        let mut tables: Vec<TableSchema> = Vec::new();
        for row in &rows {
            let schema: String = row.get("schema");
            let name: String = row.get("table_name");

            if tables
                .last()
                .is_none_or(|last| last.schema != schema || last.name != name)
            {
                tables.push(TableSchema {
                    schema,
                    name,
                    kind: match row.get::<i8, _>("kind") as u8 {
                        b'v' | b'm' => TableKind::View,
                        _ => TableKind::Table,
                    },
                    columns: Vec::new(),
                });
            }

            tables.last_mut().unwrap().columns.push(ColumnInfo {
                name: row.get("name"),
                ordinal: row.get::<i16, _>("ordinal") as i32,
                data_type: row.get("data_type"),
                nullable: row.get("nullable"),
                is_primary_key: row.get("is_primary_key"),
                default: row.get("default_expr"),
            });
        }
        Ok(tables)
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let pool = self.pools.get(&self.cfg, self.cfg.database()).await?;
            // `raw_sql` uses the simple query protocol, so a whole editor buffer of
            // statements runs as one batch — which is what a workbench needs.
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

fn result_columns(row: &PgRow) -> Vec<ColumnMeta> {
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
        "BOOL" => LogicalType::Bool,
        "INT2" | "INT4" | "INT8" | "OID" => LogicalType::Int,
        "FLOAT4" | "FLOAT8" => LogicalType::Float,
        "NUMERIC" => LogicalType::Decimal,
        // See [`value_at`] for why `CHAR` and `"CHAR"` are two different types.
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "CITEXT" | "XML" | "UNKNOWN" | "\"CHAR\"" => {
            LogicalType::Text
        }
        "UUID" => LogicalType::Uuid,
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        "TIMESTAMP" => LogicalType::Timestamp,
        "TIMESTAMPTZ" => LogicalType::TimestampTz,
        "JSON" | "JSONB" => LogicalType::Json,
        "BYTEA" => LogicalType::Binary,
        // Arrays and everything with no decoder come back as strings.
        _ => LogicalType::Unknown,
    }
}

fn row_values(row: &PgRow) -> Vec<Value> {
    (0..row.len()).map(|i| value_at(row, i)).collect()
}

/// Decode one cell into JSON, dispatching on the type sqlx reported for it.
///
/// `serde_json` numbers cannot hold arbitrary precision, so `numeric` is
/// rendered as a string rather than silently rounded through `f64`.
fn value_at(row: &PgRow, i: usize) -> Value {
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
        "BOOL" => json!(bool),
        "INT2" => json!(i16),
        "INT4" => json!(i32),
        "INT8" => json!(i64),
        "OID" => match row.try_get::<Option<sqlx::postgres::types::Oid>, _>(i) {
            Ok(Some(v)) => Value::from(v.0),
            Ok(None) => Value::Null,
            Err(e) => Value::String(format!("<decode error: {e}>")),
        },
        "FLOAT4" => json!(f32),
        "FLOAT8" => json!(f64),
        "NUMERIC" => text!(rust_decimal::Decimal),
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" | "CITEXT" | "XML" | "UNKNOWN" => json!(String),
        // Two different types, and sqlx's names for them are a trap.
        //
        // `char(n)` — the ordinary blank-padded one — is `PgType::Bpchar`, which
        // sqlx names **`CHAR`**. Postgres's internal one-byte `"char"` is
        // `PgType::Char`, which sqlx names **`"CHAR"`**, quotes included. Reading
        // the first as an `i8` failed on every value wider than a byte, so a
        // `char(2)` country code came back as `<decode error>`.
        "\"CHAR\"" => text!(i8),
        "UUID" => json!(uuid::Uuid),
        "DATE" => json!(chrono::NaiveDate),
        "TIME" => json!(chrono::NaiveTime),
        "TIMESTAMP" => json!(chrono::NaiveDateTime),
        "TIMESTAMPTZ" => json!(chrono::DateTime<chrono::Utc>),
        "JSON" | "JSONB" => json!(Value),
        "BYTEA" => match row.try_get::<Option<Vec<u8>>, _>(i) {
            Ok(Some(v)) => Value::String(hex(&v)),
            Ok(None) => Value::Null,
            Err(e) => Value::String(format!("<decode error: {e}>")),
        },
        "BOOL[]" => json!(Vec<bool>),
        "INT2[]" => json!(Vec<i16>),
        "INT4[]" => json!(Vec<i32>),
        "INT8[]" => json!(Vec<i64>),
        "FLOAT4[]" => json!(Vec<f32>),
        "FLOAT8[]" => json!(Vec<f64>),
        "TEXT[]" | "VARCHAR[]" => json!(Vec<String>),
        "UUID[]" => json!(Vec<uuid::Uuid>),
        "JSON[]" | "JSONB[]" => json!(Vec<Value>),
        // Types with no binary decoder wired up yet — `money`, `interval`, `inet`,
        // ranges, user-defined composites. Cast them in the query for now.
        other => Value::String(format!("<unsupported type {other}>")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
