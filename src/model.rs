use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The SQL dialect a source speaks. Alkyon never translates between these — the
/// editor sends the text you typed to the engine that understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    TSql,
    PgSql,
    DuckDb,
}

impl Dialect {
    /// The CodeMirror MIME type the editor should use for this dialect.
    pub fn mime(self) -> &'static str {
        match self {
            Dialect::TSql => "text/x-mssql",
            Dialect::PgSql => "text/x-pgsql",
            Dialect::DuckDb => "text/x-sql",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Postgres,
    MsSql,
}

impl SourceKind {
    pub fn default_port(self) -> u16 {
        match self {
            SourceKind::Postgres => 5432,
            SourceKind::MsSql => 1433,
        }
    }

    pub fn default_schema(self) -> &'static str {
        match self {
            SourceKind::Postgres => "public",
            SourceKind::MsSql => "dbo",
        }
    }

    /// Where to connect when no database was given. Both engines have a database
    /// that always exists and that every login can reach.
    pub fn default_database(self) -> &'static str {
        match self {
            SourceKind::Postgres => "postgres",
            SourceKind::MsSql => "master",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// No encryption.
    Disable,
    /// Encrypt if the server offers it, without validating its certificate.
    #[default]
    Prefer,
    /// Require encryption and validate the certificate against the OS trust store.
    Require,
    /// Require encryption but accept any certificate. For dev containers and
    /// on-prem servers with self-signed certificates.
    TrustCertificate,
}

/// How to authenticate. Deliberately not `Serialize`: a secret that can be
/// serialized eventually ends up in a log line or an HTTP response.
#[derive(Clone, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthConfig {
    /// SQL Server login, or PostgreSQL password authentication.
    Password { username: String, password: String },
    /// Windows integrated authentication. SQL Server on Windows hosts only.
    Integrated,
    /// A pre-acquired Microsoft Entra ID access token, for Azure SQL and Fabric.
    /// Get one with:
    /// `az account get-access-token --resource https://database.windows.net/`
    AadToken { token: String },
}

impl AuthConfig {
    pub fn method(&self) -> &'static str {
        match self {
            AuthConfig::Password { .. } => "password",
            AuthConfig::Integrated => "integrated",
            AuthConfig::AadToken { .. } => "aad_token",
        }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthConfig::Password { username, .. } => f
                .debug_struct("Password")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            AuthConfig::Integrated => f.write_str("Integrated"),
            AuthConfig::AadToken { .. } => f
                .debug_struct("AadToken")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// Where a source is defined.
///
/// The scope decides which file the record is written to and which keychain entry
/// holds its credential. Ids only have to be unique *within* a scope, so a
/// personal `warehouse` and a project's `warehouse` can both exist and both be
/// listed — which is the point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The user's own registry, in the platform config directory.
    #[default]
    User,
    /// `.alkyon/sources.json` inside the open folder — committable, since it holds
    /// no credentials.
    Project,
}

impl Scope {
    pub fn prefix(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// The non-secret half of an auth method — what `sources.json` on disk holds.
/// The secret itself lives in the OS keychain, under the source's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthKind {
    Password { username: String },
    Integrated,
    AadToken,
}

impl AuthKind {
    pub fn method(&self) -> &'static str {
        match self {
            AuthKind::Password { .. } => "password",
            AuthKind::Integrated => "integrated",
            AuthKind::AadToken => "aad_token",
        }
    }

    /// Whether using this source needs a secret from the vault.
    pub fn needs_secret(&self) -> bool {
        !matches!(self, AuthKind::Integrated)
    }
}

/// A registered source, as persisted and as served by the API: everything about
/// how to reach a server except the credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    pub id: String,
    pub kind: SourceKind,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Optional: omitted means the engine's always-present database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub auth: AuthKind,
    #[serde(default)]
    pub tls: TlsMode,
    /// Never persisted: a record's scope is the file it was loaded from, so
    /// writing it down would only create a second source of truth.
    #[serde(skip)]
    pub scope: Scope,
}

impl SourceRecord {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.kind.default_port())
    }

    /// The registry key, and the id every route takes. Qualified because two
    /// scopes may each define the same bare id.
    pub fn key(&self) -> String {
        format!("{}:{}", self.scope.prefix(), self.id)
    }

    pub fn database(&self) -> &str {
        self.database
            .as_deref()
            .unwrap_or_else(|| self.kind.default_database())
    }

    pub fn summary(&self, dialect: Dialect) -> SourceSummary {
        SourceSummary {
            key: self.key(),
            scope: self.scope,
            id: self.id.clone(),
            kind: self.kind,
            dialect,
            host: self.host.clone(),
            port: self.port(),
            instance: self.instance.clone(),
            // The effective database, so the UI never has to know the defaults.
            database: self.database().to_owned(),
            auth_method: self.auth.method(),
            tls: self.tls,
            editor_mime: dialect.mime(),
        }
    }

    /// Rejoin the record with the secret the vault gave back.
    pub fn with_secret(&self, secret: Option<String>) -> crate::error::Result<SourceConfig> {
        let auth = match (&self.auth, secret) {
            (AuthKind::Password { username }, Some(password)) => AuthConfig::Password {
                username: username.clone(),
                password,
            },
            (AuthKind::AadToken, Some(token)) => AuthConfig::AadToken { token },
            (AuthKind::Integrated, _) => AuthConfig::Integrated,
            (AuthKind::Password { .. } | AuthKind::AadToken, None) => {
                return Err(crate::error::Error::MissingSecret(self.id.clone()))
            }
        };
        Ok(SourceConfig {
            id: self.id.clone(),
            scope: self.scope,
            kind: self.kind,
            host: self.host.clone(),
            port: self.port,
            instance: self.instance.clone(),
            database: self.database.clone(),
            auth,
            tls: self.tls,
        })
    }
}

/// Everything needed to reach one database, credential included. This is the
/// wire format for `POST /sources` and the input to a `Connector`; it is never
/// stored and never serialized.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub id: String,
    /// Which registry to put it in. Defaults to the user's.
    #[serde(default)]
    pub scope: Scope,
    pub kind: SourceKind,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// SQL Server named instance, resolved through the SQL Browser service.
    /// Mutually exclusive with `port`.
    #[serde(default)]
    pub instance: Option<String>,
    /// Optional: omitted means the engine's always-present database.
    #[serde(default)]
    pub database: Option<String>,
    pub auth: AuthConfig,
    #[serde(default)]
    pub tls: TlsMode,
}

impl SourceConfig {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.kind.default_port())
    }

    pub fn database(&self) -> &str {
        self.database
            .as_deref()
            .unwrap_or_else(|| self.kind.default_database())
    }

    /// Peel the credential off, leaving the part that is safe to write down.
    pub fn split(self) -> (SourceRecord, Option<String>) {
        let (auth, secret) = match self.auth {
            AuthConfig::Password { username, password } => {
                (AuthKind::Password { username }, Some(password))
            }
            AuthConfig::Integrated => (AuthKind::Integrated, None),
            AuthConfig::AadToken { token } => (AuthKind::AadToken, Some(token)),
        };
        let record = SourceRecord {
            id: self.id,
            scope: self.scope,
            kind: self.kind,
            host: self.host,
            port: self.port,
            instance: self.instance,
            database: self.database,
            auth,
            tls: self.tls,
        };
        (record, secret)
    }
}

/// The secret-free view of a source, safe to hand to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSummary {
    /// `user:<id>` or `project:<id>` — what the routes and the UI address.
    pub key: String,
    pub scope: Scope,
    pub id: String,
    pub kind: SourceKind,
    pub dialect: Dialect,
    pub host: String,
    pub port: u16,
    pub instance: Option<String>,
    pub database: String,
    pub auth_method: &'static str,
    pub tls: TlsMode,
    /// Which CodeMirror mode the editor should switch to for this source. The
    /// backend owns dialect knowledge; the UI just applies what it is told.
    pub editor_mime: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub ordinal: i32,
    /// The engine's own type spelling, e.g. `numeric(18,2)` or `nvarchar(50)`.
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub default: Option<String>,
}

/// A table with its columns — one entry of a schema snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct TableSchema {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnInfo>,
}

impl TableSchema {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

/// A dialect-independent description of what a result column holds.
///
/// The JSON a row travels as is ambiguous on its own: `numeric` is deliberately
/// rendered as a *string* to keep its digits, and so is `varchar`. Federation has
/// to tell them apart to build a typed DuckDB table, so each connector reports
/// this alongside its own type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Bool,
    Int,
    Float,
    /// Arrives as a string; exact digits preserved.
    Decimal,
    Text,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    /// Arrives as a hex string.
    Binary,
    Unknown,
}

/// A column of a result set, as opposed to a column of a table.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnMeta {
    pub name: String,
    /// The engine's own spelling, shown in the grid's column tooltip.
    pub type_name: String,
    pub logical: LogicalType,
}

/// One item of a streamed result.
///
/// A statement that returns rows emits `Columns` once, then `Rows` repeatedly.
/// A statement that returns none emits `Affected`. A batch of several statements
/// emits that sequence once per statement.
#[derive(Debug, Clone)]
pub enum RowBatch {
    Columns(Arc<Vec<ColumnMeta>>),
    Rows(Vec<Vec<Value>>),
    Affected(u64),
}

/// Rows accumulated before a batch is pushed to the client.
pub const BATCH_ROWS: usize = 500;
