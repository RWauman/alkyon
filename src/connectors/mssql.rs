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

    match cfg.instance.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        // A hostname in this field is the paste that costs the most to diagnose:
        // naming an instance sends the connection to the SQL Browser service on
        // UDP 1434, which no cloud endpoint runs, and the failure that comes back
        // is a browser timeout naming a host that is perfectly reachable.
        Some(instance) if instance.contains('.') || instance.contains('\\') => {
            return Err(crate::error::Error::BadRequest(format!(
                "`{instance}` is a hostname, not a named instance. An instance is a bare \
                 name like `SQLEXPRESS`, resolved by the SQL Browser service on a local \
                 network. Azure SQL and Fabric are reached by hostname on port 1433 — put \
                 it in Host and leave Named instance empty."
            )))
        }
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
        AuthConfig::AadToken { token } => {
            // Which application the token was issued to is the one thing a
            // server can refuse that nothing else reveals. Claims only.
            tracing::debug!(
                host = %cfg.host,
                claims = %crate::azure::entra::describe_token(token),
                "logging in with an Entra token"
            );
            AuthMethod::aad_token(token)
        }
        #[cfg(windows)]
        AuthConfig::Integrated => AuthMethod::Integrated,
        #[cfg(not(windows))]
        AuthConfig::Integrated => {
            return Err(crate::error::Error::Unsupported(
                "Windows integrated authentication is only available on Windows hosts".into(),
            ))
        }
        // Never reaches a connector: a sign-in is exchanged for an access token
        // before the connection is opened. See `AppState::authorise`.
        AuthConfig::Entra { .. } => {
            return Err(crate::error::Error::Unsupported(
                "the Entra sign-in was not exchanged for an access token".into(),
            ))
        }
        AuthConfig::None => {
            return Err(crate::error::Error::Unsupported(
                "SQL Server needs a login, Windows integrated authentication or an Entra ID token"
                    .into(),
            ))
        }
    });

    Ok(c)
}

/// The connection the server asked for instead of the one we made: the address
/// to dial, and the configuration to log in with.
///
/// A routed name may carry an instance —
/// `cluster.pbidedicated.windows.net\WORKSPACE-dw` — and the two halves are used
/// for **different things**, which is the whole subtlety here:
///
/// - the part before the backslash is the host to open a socket to, and the name
///   the certificate is checked against;
/// - the **whole** name, instance included, is what the login packet would have
///   to carry, and `tiberius` cannot say both: `Config::host` is the certificate's
///   name *and* the login's. Sending the host alone gets a Fabric login closed
///   without a word, so a redirect carrying an instance is refused here with a
///   sentence instead. See *What does not work yet* in the guide.
///
/// Azure SQL is unaffected: it routes to a host and a port, with no instance, so
/// the two names agree and there is nothing to choose between.
///
/// The instance is never handed to the SQL Browser either: the routing token
/// brings its own port, so there is nothing to resolve, and asking would be a
/// UDP timeout against a service no cloud endpoint runs.
fn routed_config(cfg: &SourceConfig, db: &str, alternative: &str, port: u16) -> Result<Routed> {
    let (host, instance) = match alternative.split_once('\\') {
        Some((host, instance)) => (host, instance),
        None => (alternative, ""),
    };
    if host.is_empty() {
        return Err(crate::error::Error::BadRequest(format!(
            "the server redirected this connection to `{alternative}`, which is not an \
             address that can be connected to"
        )));
    }

    if !instance.is_empty() {
        return Err(crate::error::Error::Unsupported(format!(
            "`{}` redirects this connection to `{alternative}`, and the login would have to \
             carry that whole name — instance included — for the server to know which \
             database is meant. The driver alkyon uses takes one name for both the login \
             and the certificate, so it cannot send it. This is why a Microsoft Fabric SQL \
             endpoint does not connect; the guide says more. Azure SQL is unaffected.",
            cfg.host
        )));
    }

    let config = config_for(
        &SourceConfig {
            host: host.to_owned(),
            port: Some(port),
            instance: None,
            ..cfg.clone()
        },
        db,
    )?;

    Ok(Routed {
        addr: config.get_addr(),
        config,
    })
}

/// Where a redirect sends us.
#[derive(Debug)]
struct Routed {
    /// The address to dial. Kept out of the configuration so it can be asserted
    /// on, and logged, without reading the driver's own state back.
    addr: String,
    config: Config,
}

/// Open a fresh session. SQL Server connections are cheap enough for a personal
/// tool, and not pooling them means a session can never go stale between
/// requests. Pooling can be added later without changing the trait.
///
/// **One redirect is followed.** Azure SQL and Fabric answer the login with a
/// routing token pointing at the node that actually holds the database — for a
/// Fabric endpoint that is every single time — and tiberius reports it as an
/// error for the caller to act on rather than following it itself. Only one,
/// and never in a loop: a server that keeps redirecting is a server to give up
/// on rather than to chase.
async fn open(cfg: &SourceConfig, db: &str) -> Result<Session> {
    let config = config_for(cfg, db)?;
    let tcp = if cfg.instance.is_some() {
        use tiberius::SqlBrowser;
        TcpStream::connect_named(&config).await?
    } else {
        TcpStream::connect(config.get_addr()).await?
    };
    tcp.set_nodelay(true)?;

    match Client::connect(config, tcp.compat_write()).await {
        Ok(session) => Ok(session),
        Err(tiberius::error::Error::Routing { host, port }) => {
            tracing::debug!(%host, port, "following the server's routing token");
            let routed = routed_config(cfg, db, &host, port)?;
            let addr = routed.addr;
            tracing::debug!(dialling = %addr, "reconnecting to the routed node");

            // Both legs otherwise fail with the same sentence, and which one
            // gave way is the whole question when a redirect is involved.
            let tcp = TcpStream::connect(&addr).await.map_err(|e| {
                crate::error::Error::BadRequest(format!(
                    "`{}` redirected this connection to `{addr}`, which cannot be reached: {e}",
                    cfg.host
                ))
            })?;
            tcp.set_nodelay(true)?;
            Client::connect(routed.config, tcp.compat_write())
                .await
                .map_err(|e| {
                    crate::error::Error::BadRequest(format!(
                        "`{}` redirected this connection to `{addr}`, which then refused it: {}",
                        cfg.host,
                        explain(e, db)
                    ))
                })
        }
        Err(e) => Err(explain(e, db)),
    }
}

/// Turn SQL Server's most misread error into what it means.
///
/// Asked for a database that is not there, SQL Server answers 4060: *Cannot open
/// database "x" requested by the login. The login failed.* It says the database
/// first and the login last, and the login is what people read — so a typo in a
/// database name is spent looking at credentials. The server genuinely does not
/// distinguish "no such database" from "you may not open it", and the message
/// should not pretend otherwise.
fn explain(error: tiberius::error::Error, db: &str) -> crate::error::Error {
    if let tiberius::error::Error::Server(token) = &error {
        if token.code() == 4060 {
            return crate::error::Error::BadRequest(format!(
                "cannot open database `{db}` — it does not exist on this server, or this login \
                 may not open it. SQL Server reports both the same way, as a login failure."
            ));
        }
    }
    error.into()
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

pub(crate) const LIST_DATABASES: &str = "\
SELECT name
FROM sys.databases
WHERE state = 0 AND HAS_DBACCESS(name) = 1
ORDER BY name";

pub(crate) const LIST_TABLES: &str = "\
SELECT s.name AS [schema], o.name AS [name], o.type AS [kind]
FROM sys.objects o
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE o.type IN ('U', 'V')
ORDER BY s.name, o.name";

pub(crate) const LIST_COLUMNS: &str = "\
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
pub(crate) const SNAPSHOT: &str = "\
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
pub(crate) fn spell_type(name: &str, max_length: i16, precision: u8, scale: u8) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Scope, SourceKind};

    fn azure(instance: Option<&str>) -> SourceConfig {
        SourceConfig {
            id: "fabric".to_owned(),
            scope: Scope::User,
            kind: SourceKind::MsSql,
            host: "abc-def.datawarehouse.fabric.microsoft.com".to_owned(),
            path: None,
            options: Default::default(),
            port: None,
            instance: instance.map(str::to_owned),
            database: Some("lh_bronze".to_owned()),
            auth: AuthConfig::AadToken {
                token: "t".to_owned(),
            },
            // Encrypted, certificate not checked — what a redirect to a name
            // carrying an instance needs, and what the dialogue defaults to.
            tls: TlsMode::Prefer,
        }
    }

    /// The hostname pasted into *Named instance*, which sends the connection to
    /// the SQL Browser service on UDP 1434 — a thing no cloud endpoint runs, and
    /// whose timeout names a host that is in fact perfectly reachable.
    #[test]
    fn a_hostname_is_not_a_named_instance() {
        let error = config_for(&azure(Some("abc-def.datawarehouse.fabric.microsoft.com")), "db")
            .expect_err("a hostname here can only time out");
        let said = error.to_string();
        assert!(said.contains("Named instance empty"), "{said}");

        // On-prem's own spelling is refused too: the host belongs in Host.
        assert!(config_for(&azure(Some(r"SERVER\SQLEXPRESS")), "db").is_err());
    }

    #[test]
    fn a_real_instance_still_goes_through_the_browser() {
        assert!(config_for(&azure(Some("SQLEXPRESS")), "db").is_ok());
    }

    /// A field left blank is not an instance called "".
    #[test]
    fn an_empty_instance_is_no_instance() {
        for blank in [None, Some(""), Some("   ")] {
            assert!(config_for(&azure(blank), "db").is_ok(), "{blank:?}");
        }
    }

    /// An Azure SQL redirect: a host and a port, nothing to choose between, and
    /// the connection simply moves.
    #[test]
    fn a_routed_address_without_an_instance_is_followed() {
        let routed =
            routed_config(&azure(None), "db", "node-3.database.windows.net", 11003).unwrap();
        assert_eq!(routed.addr, "node-3.database.windows.net:11003");
        assert_eq!(routed.config.get_addr(), routed.addr);
    }

    /// A Fabric redirect: the login would have to carry the instance, the driver
    /// cannot say two names, and that is said rather than half-attempted. It was
    /// half-attempted, and the server closed the connection without a word.
    #[test]
    fn a_routed_address_carrying_an_instance_is_refused_with_the_reason() {
        let full = r"pbipswissn2-switzerlandnorth.pbidedicated.windows.net\7TF6KDG7-AQNJJHAC-dw";
        let refused = routed_config(&azure(None), "db", full, 1433)
            .expect_err("the instance cannot be sent, so this must not look like it worked");

        let said = refused.to_string();
        assert!(said.contains("Fabric"), "{said}");
        assert!(said.contains(full), "{said}");
    }

    /// Following a redirect must not quietly cost the certificate check — which
    /// it would if the whole routed name were pushed through the field the
    /// certificate is verified against.
    #[test]
    fn following_a_redirect_keeps_the_certificate_verified() {
        let mut verifying = azure(None);
        verifying.tls = TlsMode::Require;

        let routed = routed_config(&verifying, "db", "node-3.database.windows.net", 11003)
            .expect("a plain redirect is followable with the certificate still checked");
        assert_eq!(routed.addr, "node-3.database.windows.net:11003");
        assert_eq!(routed.config.get_addr(), routed.addr);
    }

    #[test]
    fn a_redirect_to_nowhere_is_refused() {
        assert!(routed_config(&azure(None), "db", r"\instance-only", 1433).is_err());
    }
}
