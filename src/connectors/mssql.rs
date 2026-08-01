use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::TryStreamExt;
use serde_json::Value;
use std::sync::Arc;
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, QueryItem, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{Connection, Connector};
use crate::error::Result;
use crate::model::{
    AuthConfig, ColumnInfo, ColumnMeta, Dialect, LogicalType, RowBatch, SourceConfig, TableInfo,
    TableKind, TableSchema, TlsMode, BATCH_ROWS,
};

type Session = Client<Compat<TcpStream>>;

fn config_for(cfg: &SourceConfig, db: &str) -> Result<Config> {
    let mut c = Config::new();
    c.host(&cfg.host);
    c.database(db);
    c.application_name("alkyon");

    match &cfg.instance {
        // The SQL Browser service resolves the instance to a dynamic port.
        Some(instance) => c.instance_name(instance),
        None => c.port(cfg.port()),
    }

    match cfg.tls {
        TlsMode::Disable => c.encryption(EncryptionLevel::NotSupported),
        TlsMode::Prefer => {
            c.encryption(EncryptionLevel::On);
            c.trust_cert();
        }
        TlsMode::Require => c.encryption(EncryptionLevel::Required),
        TlsMode::TrustCertificate => {
            c.encryption(EncryptionLevel::Required);
            c.trust_cert();
        }
    }

    c.authentication(match &cfg.auth {
        AuthConfig::Password { username, password } => AuthMethod::sql_server(username, password),
        AuthConfig::AadToken { token } => AuthMethod::aad_token(token),
        #[cfg(windows)]
        AuthConfig::Integrated => AuthMethod::Integrated,
        #[cfg(not(windows))]
        AuthConfig::Integrated => {
            return Err(crate::error::Error::Unsupported(
                "Windows integrated authentication is only available on Windows hosts".into(),
            ))
        }
    });

    Ok(c)
}

/// Open a fresh session. SQL Server connections are cheap enough for a personal
/// tool, and not pooling them means a session can never go stale between
/// requests. Pooling can be added later without changing the trait.
async fn open(cfg: &SourceConfig, db: &str) -> Result<Session> {
    let config = config_for(cfg, db)?;
    let tcp = if cfg.instance.is_some() {
        use tiberius::SqlBrowser;
        TcpStream::connect_named(&config).await?
    } else {
        TcpStream::connect(config.get_addr()).await?
    };
    tcp.set_nodelay(true)?;
    Ok(Client::connect(config, tcp.compat_write()).await?)
}

#[derive(Default)]
pub struct MssqlConnector;

#[async_trait]
impl Connector for MssqlConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        // Prove the credentials work before the source is registered.
        open(config, config.database()).await?;
        Ok(Box::new(MssqlConnection {
            cfg: config.clone(),
        }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::TSql
    }
}

pub struct MssqlConnection {
    cfg: SourceConfig,
}

const LIST_DATABASES: &str = "\
SELECT name
FROM sys.databases
WHERE state = 0 AND HAS_DBACCESS(name) = 1
ORDER BY name";

const LIST_TABLES: &str = "\
SELECT s.name AS [schema], o.name AS [name], o.type AS [kind]
FROM sys.objects o
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE o.type IN ('U', 'V')
ORDER BY s.name, o.name";

const LIST_COLUMNS: &str = "\
SELECT c.name        AS [name],
       c.column_id   AS [ordinal],
       t.name        AS [type_name],
       c.max_length  AS [max_length],
       c.precision   AS [precision],
       c.scale       AS [scale],
       c.is_nullable AS [nullable],
       dc.definition AS [default_expr],
       CAST(CASE WHEN EXISTS (
           SELECT 1
           FROM sys.index_columns ic
           JOIN sys.indexes ix ON ix.object_id = ic.object_id AND ix.index_id = ic.index_id
           WHERE ic.object_id = c.object_id
             AND ic.column_id = c.column_id
             AND ix.is_primary_key = 1
       ) THEN 1 ELSE 0 END AS bit) AS [is_primary_key]
FROM sys.columns c
JOIN sys.objects o ON o.object_id = c.object_id
JOIN sys.schemas s ON s.schema_id = o.schema_id
JOIN sys.types t   ON t.user_type_id = c.user_type_id
LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
WHERE s.name = @P1 AND o.name = @P2
ORDER BY c.column_id";

/// `LIST_COLUMNS` widened to the whole database, carrying the object type so a
/// snapshot costs one round trip rather than one per table.
const SNAPSHOT: &str = "\
SELECT s.name        AS [schema],
       o.name        AS [table_name],
       o.type        AS [kind],
       c.name        AS [name],
       c.column_id   AS [ordinal],
       t.name        AS [type_name],
       c.max_length  AS [max_length],
       c.precision   AS [precision],
       c.scale       AS [scale],
       c.is_nullable AS [nullable],
       dc.definition AS [default_expr],
       CAST(CASE WHEN EXISTS (
           SELECT 1
           FROM sys.index_columns ic
           JOIN sys.indexes ix ON ix.object_id = ic.object_id AND ix.index_id = ic.index_id
           WHERE ic.object_id = c.object_id
             AND ic.column_id = c.column_id
             AND ix.is_primary_key = 1
       ) THEN 1 ELSE 0 END AS bit) AS [is_primary_key]
FROM sys.columns c
JOIN sys.objects o ON o.object_id = c.object_id
JOIN sys.schemas s ON s.schema_id = o.schema_id
JOIN sys.types t   ON t.user_type_id = c.user_type_id
LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
WHERE o.type IN ('U', 'V')
ORDER BY s.name, o.name, c.column_id";

/// One row of `LIST_COLUMNS` or `SNAPSHOT`.
fn column_from(row: &Row) -> ColumnInfo {
    ColumnInfo {
        name: row.get::<&str, _>("name").unwrap_or_default().to_owned(),
        ordinal: row.get::<i32, _>("ordinal").unwrap_or_default(),
        data_type: spell_type(
            row.get::<&str, _>("type_name").unwrap_or_default(),
            row.get::<i16, _>("max_length").unwrap_or_default(),
            row.get::<u8, _>("precision").unwrap_or_default(),
            row.get::<u8, _>("scale").unwrap_or_default(),
        ),
        nullable: row.get::<bool, _>("nullable").unwrap_or(true),
        is_primary_key: row.get::<bool, _>("is_primary_key").unwrap_or_default(),
        default: row.get::<&str, _>("default_expr").map(str::to_owned),
    }
}

#[async_trait]
impl Connection for MssqlConnection {
    async fn list_databases(&self) -> Result<Vec<String>> {
        let mut session = open(&self.cfg, self.cfg.database()).await?;
        let rows = session
            .simple_query(LIST_DATABASES)
            .await?
            .into_first_result()
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get::<&str, _>("name").map(str::to_owned))
            .collect())
    }

    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>> {
        // Azure SQL and Fabric forbid cross-database queries, so browse another
        // database by connecting to it rather than with a three-part name.
        let mut session = open(&self.cfg, db).await?;
        let rows = session
            .simple_query(LIST_TABLES)
            .await?
            .into_first_result()
            .await?;
        Ok(rows
            .iter()
            .map(|r| TableInfo {
                schema: r.get::<&str, _>("schema").unwrap_or_default().to_owned(),
                name: r.get::<&str, _>("name").unwrap_or_default().to_owned(),
                kind: match r.get::<&str, _>("kind").unwrap_or_default().trim() {
                    "V" => TableKind::View,
                    _ => TableKind::Table,
                },
            })
            .collect())
    }

    async fn list_columns(&self, db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let mut session = open(&self.cfg, db).await?;
        let rows = session
            .query(LIST_COLUMNS, &[&schema, &table])
            .await?
            .into_first_result()
            .await?;
        Ok(rows.iter().map(column_from).collect())
    }

    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>> {
        let mut session = open(&self.cfg, db).await?;
        let rows = session
            .simple_query(SNAPSHOT)
            .await?
            .into_first_result()
            .await?;

        // Rows arrive grouped by object, so a running "current table" is enough.
        let mut tables: Vec<TableSchema> = Vec::new();
        for row in &rows {
            let schema = row.get::<&str, _>("schema").unwrap_or_default();
            let name = row.get::<&str, _>("table_name").unwrap_or_default();

            if tables
                .last()
                .is_none_or(|last| last.schema != schema || last.name != name)
            {
                tables.push(TableSchema {
                    schema: schema.to_owned(),
                    name: name.to_owned(),
                    kind: match row.get::<&str, _>("kind").unwrap_or_default().trim() {
                        "V" => TableKind::View,
                        _ => TableKind::Table,
                    },
                    columns: Vec::new(),
                });
            }

            tables.last_mut().unwrap().columns.push(column_from(row));
        }
        Ok(tables)
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let mut session = open(&self.cfg, self.cfg.database()).await?;
            // `simple_query` sends the text as a raw batch, so DDL, `GO`-free
            // multi-statement scripts and `SET` options all behave normally.
            let mut stream = session.simple_query(sql).await?;
            let mut buffered: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);

            while let Some(item) = stream.try_next().await? {
                match item {
                    // A new result set begins.
                    QueryItem::Metadata(meta) => {
                        if !buffered.is_empty() {
                            yield RowBatch::Rows(std::mem::take(&mut buffered));
                        }
                        let columns: Vec<ColumnMeta> = meta
                            .columns()
                            .iter()
                            .map(|c| ColumnMeta {
                                name: c.name().to_owned(),
                                type_name: format!("{:?}", c.column_type()).to_lowercase(),
                                logical: logical_type(c.column_type()),
                            })
                            .collect();
                        yield RowBatch::Columns(Arc::new(columns));
                    }
                    QueryItem::Row(row) => {
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

/// Rebuild the type as a T-SQL author would write it: `nvarchar(50)`,
/// `decimal(18,2)`, `varbinary(max)`.
fn spell_type(name: &str, max_length: i16, precision: u8, scale: u8) -> String {
    let length = |halved: bool| -> String {
        if max_length < 0 {
            "max".to_owned()
        } else if halved {
            // `max_length` counts bytes, and Unicode types use two per character.
            (max_length / 2).to_string()
        } else {
            max_length.to_string()
        }
    };
    match name {
        "nvarchar" | "nchar" => format!("{name}({})", length(true)),
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", length(false)),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "datetimeoffset" | "time" => format!("{name}({scale})"),
        _ => name.to_owned(),
    }
}

/// Mirrors the arms of [`value_at`]: whatever that renders, this names.
fn logical_type(column: ColumnType) -> LogicalType {
    match column {
        ColumnType::Null => LogicalType::Unknown,
        ColumnType::Bit | ColumnType::Bitn => LogicalType::Bool,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => LogicalType::Int,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => LogicalType::Float,
        // `money` is fixed-scale, but tiberius hands it over as f64 already.
        ColumnType::Money | ColumnType::Money4 => LogicalType::Float,
        ColumnType::Decimaln | ColumnType::Numericn => LogicalType::Decimal,
        ColumnType::Guid => LogicalType::Uuid,
        ColumnType::Daten => LogicalType::Date,
        ColumnType::Timen => LogicalType::Time,
        ColumnType::Datetime
        | ColumnType::Datetime2
        | ColumnType::Datetime4
        | ColumnType::Datetimen => LogicalType::Timestamp,
        ColumnType::DatetimeOffsetn => LogicalType::TimestampTz,
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText
        | ColumnType::Xml => LogicalType::Text,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => LogicalType::Binary,
        _ => LogicalType::Unknown,
    }
}

fn row_values(row: &Row) -> Vec<Value> {
    (0..row.columns().len()).map(|i| value_at(row, i)).collect()
}

fn value_at(row: &Row, i: usize) -> Value {
    macro_rules! json {
        ($t:ty) => {
            match row.try_get::<$t, _>(i) {
                Ok(Some(v)) => serde_json::to_value(v).unwrap_or(Value::Null),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        };
    }
    macro_rules! text {
        ($t:ty) => {
            match row.try_get::<$t, _>(i) {
                Ok(Some(v)) => Value::String(v.to_string()),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        };
    }

    match row.columns()[i].column_type() {
        ColumnType::Null => Value::Null,
        ColumnType::Bit | ColumnType::Bitn => json!(bool),
        ColumnType::Int1 => json!(u8),
        ColumnType::Int2 => json!(i16),
        ColumnType::Int4 => json!(i32),
        ColumnType::Int8 => json!(i64),
        // `intn` covers every nullable integer width; try them widest-first.
        ColumnType::Intn => any_int(row, i),
        ColumnType::Float4 => json!(f32),
        ColumnType::Float8 | ColumnType::Money | ColumnType::Money4 => json!(f64),
        ColumnType::Floatn => match row.try_get::<f64, _>(i) {
            Ok(Some(v)) => serde_json::to_value(v).unwrap_or(Value::Null),
            Ok(None) => Value::Null,
            Err(_) => json!(f32),
        },
        // Scale is fixed, so keep full precision as text instead of rounding.
        ColumnType::Decimaln | ColumnType::Numericn => text!(rust_decimal::Decimal),
        ColumnType::Guid => text!(uuid::Uuid),
        ColumnType::Daten => json!(chrono::NaiveDate),
        ColumnType::Timen => json!(chrono::NaiveTime),
        ColumnType::Datetime
        | ColumnType::Datetime2
        | ColumnType::Datetime4
        | ColumnType::Datetimen => json!(chrono::NaiveDateTime),
        ColumnType::DatetimeOffsetn => json!(chrono::DateTime<chrono::Utc>),
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText
        | ColumnType::Xml => json!(&str),
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            match row.try_get::<&[u8], _>(i) {
                Ok(Some(v)) => Value::String(hex(v)),
                Ok(None) => Value::Null,
                Err(e) => Value::String(format!("<decode error: {e}>")),
            }
        }
        other => Value::String(format!("<unsupported type {other:?}>")),
    }
}

fn any_int(row: &Row, i: usize) -> Value {
    if let Ok(v) = row.try_get::<i32, _>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<i16, _>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    match row.try_get::<u8, _>(i) {
        Ok(Some(v)) => Value::from(v),
        Ok(None) => Value::Null,
        Err(e) => Value::String(format!("<decode error: {e}>")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    s
}
