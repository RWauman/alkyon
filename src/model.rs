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
    MySql,
    DuckDb,
}

impl Dialect {
    /// The CodeMirror MIME type the editor should use for this dialect.
    pub fn mime(self) -> &'static str {
        match self {
            Dialect::TSql => "text/x-mssql",
            Dialect::PgSql => "text/x-pgsql",
            Dialect::MySql => "text/x-mysql",
            Dialect::DuckDb => "text/x-sql",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Postgres,
    MsSql,
    MySql,
    /// A folder of data files on the machine running alkyon, read by DuckDB. It
    /// has a `path` instead of a host, and no credential.
    Folder,
    /// One data file. The same reader as [`SourceKind::Folder`], but the sandbox
    /// is granted that file alone rather than the directory around it, and the
    /// format options describe *this* file rather than a folder's worth.
    File,
}

impl SourceKind {
    /// Whether this kind is reached over the network. The false case is what
    /// makes `host`, `port`, encryption and authentication meaningless.
    pub fn is_server(self) -> bool {
        !matches!(self, SourceKind::Folder | SourceKind::File)
    }

    /// Whether this kind is read off the local filesystem.
    pub fn is_files(self) -> bool {
        !self.is_server()
    }

    pub fn default_port(self) -> u16 {
        match self {
            SourceKind::Postgres => 5432,
            SourceKind::MsSql => 1433,
            SourceKind::MySql => 3306,
            // Not a port at all. Reported as 0 and hidden by the UI, rather than
            // making every summary carry an `Option` for one kind's sake.
            SourceKind::Folder | SourceKind::File => 0,
        }
    }

    pub fn default_schema(self) -> &'static str {
        match self {
            SourceKind::Postgres => "public",
            SourceKind::MsSql => "dbo",
            // MySQL has no schema layer: a schema *is* a database. There is
            // therefore no default to give, and the connector reads an empty
            // schema as "the database this call names".
            SourceKind::MySql => "",
            // Every table a folder or file source exposes. `public` rather than
            // DuckDB's own `main`, because that is the name a SQL user expects.
            SourceKind::Folder | SourceKind::File => "public",
        }
    }

    /// Where to connect when no database was given. Every engine has one that
    /// always exists and that every login can reach.
    pub fn default_database(self) -> &'static str {
        match self {
            SourceKind::Postgres => "postgres",
            SourceKind::MsSql => "master",
            // Readable by everyone and always present, and privilege-filtered by
            // the server so it shows only what this login may see.
            SourceKind::MySql => "information_schema",
            // What DuckDB itself calls an in-memory catalogue. Only the fallback
            // for a path with no name of its own — see [`catalogue_name`].
            SourceKind::Folder | SourceKind::File => "memory",
        }
    }
}

/// The catalogue a folder or file source shows where a server shows a database.
///
/// DuckDB's own in-memory catalogue is called `memory`, which says nothing about
/// what you are looking at, so the folder standing behind the source lends its
/// name: `parquet` → `public` → `customers`. A file source shows the directory it
/// sits in, since its own name is already the table's.
pub fn catalogue_name(kind: SourceKind, path: &std::path::Path) -> String {
    let directory = if kind == SourceKind::File {
        path.parent()
    } else {
        Some(path)
    };
    directory
        .and_then(|directory| directory.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| kind.default_database().to_owned())
}

/// What a data file holds, and therefore which reader opens it.
///
/// Every one of these is available without fetching anything: CSV is core DuckDB,
/// parquet and JSON are linked in by Cargo features, and Excel is read in-process
/// by calamine rather than by DuckDB's `excel` extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Csv,
    Parquet,
    /// A JSON document, or an array of them.
    Json,
    /// One JSON document per line — `.jsonl`, `.ndjson`.
    JsonLines,
    /// A spreadsheet. One table per sheet.
    Excel,
}

impl FileFormat {
    /// The extensions that mean this format when nothing was declared.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            FileFormat::Csv => &["csv", "tsv", "txt"],
            FileFormat::Parquet => &["parquet"],
            FileFormat::Json => &["json"],
            FileFormat::JsonLines => &["jsonl", "ndjson"],
            FileFormat::Excel => &["xlsx", "xlsm", "xlsb", "xls"],
        }
    }

    pub const ALL: &'static [FileFormat] = &[
        FileFormat::Csv,
        FileFormat::Parquet,
        FileFormat::Json,
        FileFormat::JsonLines,
        FileFormat::Excel,
    ];

    /// The format an extension implies, or `None` for one alkyon does not read.
    pub fn from_extension(extension: &str) -> Option<Self> {
        let lower = extension.to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.extensions().contains(&lower.as_str()))
    }

    /// Whether the format is read by calamine rather than by DuckDB.
    pub fn is_excel(self) -> bool {
        matches!(self, FileFormat::Excel)
    }
}

/// How to read a CSV, for the times its shape is not what a sniffer would guess.
///
/// Every field is optional and every absent field means "let DuckDB work it out" —
/// its sniffer is good, and overriding it wholesale would make a European CSV work
/// at the cost of making every other one worse. What these exist for is the file
/// the sniffer gets wrong: `;` delimited with `,` decimals, or Latin-1, or three
/// lines of preamble above the header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsvOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escape: Option<String>,
    /// `false` names the columns `column0`, `column1`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<bool>,
    /// `,` for `1 234,56`. DuckDB calls this `decimal_separator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal: Option<String>,
    /// `utf-8`, `utf-16` or `latin-1` — what DuckDB itself accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The text that means NULL, beyond an empty field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_string: Option<String>,
    /// Lines to drop before the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_format: Option<String>,
    /// Keep going past a row that will not parse, instead of failing the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_errors: Option<bool>,
    /// How many rows the sniffer reads before deciding the types. `-1` is all of
    /// them, which is the cure for a column that is integers for 20 000 rows and
    /// then a word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_size: Option<i64>,
    /// Read every column as text. The escape hatch when nothing else works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_varchar: Option<bool>,
}

/// How to read a spreadsheet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcelOptions {
    /// Only this sheet, rather than one table per sheet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
}

/// The format half of a folder or file source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOptions {
    /// What the files hold. A folder source must declare it — that is what lets a
    /// directory's files be read as one table — and a file source may, which is
    /// how `export.dat` gets read as CSV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FileFormat>,
    /// Files to leave out, relative to the folder and with forward slashes.
    /// Empty — the default — reads every file of the declared format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default_csv")]
    pub csv: CsvOptions,
    #[serde(default, skip_serializing_if = "is_default_excel")]
    pub excel: ExcelOptions,
}

fn is_default_csv(options: &CsvOptions) -> bool {
    options == &CsvOptions::default()
}

fn is_default_excel(options: &ExcelOptions) -> bool {
    options == &ExcelOptions::default()
}

fn is_default_options(options: &FileOptions) -> bool {
    options == &FileOptions::default()
}

impl FileOptions {
    /// The format for a file with this extension: what was declared, or what the
    /// extension says.
    pub fn format_for(&self, extension: Option<&str>) -> Option<FileFormat> {
        self.format
            .or_else(|| extension.and_then(FileFormat::from_extension))
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
    /// Nothing to authenticate: a folder or file source is reached through the
    /// filesystem, with whatever rights the alkyon process already has.
    None,
}

impl AuthConfig {
    pub fn method(&self) -> &'static str {
        match self {
            AuthConfig::Password { .. } => "password",
            AuthConfig::Integrated => "integrated",
            AuthConfig::AadToken { .. } => "aad_token",
            AuthConfig::None => "none",
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
            AuthConfig::None => f.write_str("None"),
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
    None,
}

impl AuthKind {
    pub fn method(&self) -> &'static str {
        match self {
            AuthKind::Password { .. } => "password",
            AuthKind::Integrated => "integrated",
            AuthKind::AadToken => "aad_token",
            AuthKind::None => "none",
        }
    }

    /// Whether using this source needs a secret from the vault.
    pub fn needs_secret(&self) -> bool {
        !matches!(self, AuthKind::Integrated | AuthKind::None)
    }
}

/// A registered source, as persisted and as served by the API: everything about
/// how to reach a server except the credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    pub id: String,
    pub kind: SourceKind,
    /// Empty for a folder or file source, which has a `path` instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// The folder or file a [`SourceKind::Folder`] / [`SourceKind::File`] source
    /// points at, as typed — `~` and all. Resolved when the connection opens, not
    /// when it is saved, so a project registry stays portable between machines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How to read those files. Empty for a server source.
    #[serde(default, skip_serializing_if = "is_default_options")]
    pub options: FileOptions,
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

    /// The database this source answers about — derived rather than static for a
    /// folder or file source, whose catalogue is named after its folder.
    pub fn database(&self) -> String {
        if let Some(database) = self.database.as_deref().filter(|db| !db.is_empty()) {
            return database.to_owned();
        }
        match self.path.as_deref() {
            Some(path) if self.kind.is_files() => {
                catalogue_name(self.kind, std::path::Path::new(path))
            }
            _ => self.kind.default_database().to_owned(),
        }
    }

    pub fn summary(&self, dialect: Dialect) -> SourceSummary {
        SourceSummary {
            key: self.key(),
            scope: self.scope,
            id: self.id.clone(),
            kind: self.kind,
            dialect,
            host: self.host.clone(),
            path: self.path.clone(),
            options: self.options.clone(),
            port: self.port(),
            instance: self.instance.clone(),
            // The effective database, so the UI never has to know the defaults.
            database: self.database(),
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
            (AuthKind::None, _) => AuthConfig::None,
            (AuthKind::Password { .. } | AuthKind::AadToken, None) => {
                return Err(crate::error::Error::MissingSecret(self.id.clone()))
            }
        };
        Ok(SourceConfig {
            id: self.id.clone(),
            scope: self.scope,
            kind: self.kind,
            host: self.host.clone(),
            path: self.path.clone(),
            options: self.options.clone(),
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
    /// Not sent by a folder or file source, which sends `path` instead.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub options: FileOptions,
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
            AuthConfig::None => (AuthKind::None, None),
        };
        let record = SourceRecord {
            id: self.id,
            scope: self.scope,
            kind: self.kind,
            host: self.host,
            path: self.path,
            options: self.options,
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
    /// Set only for a folder or file source; the UI shows it where the others
    /// show `host:port`.
    pub path: Option<String>,
    pub options: FileOptions,
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
