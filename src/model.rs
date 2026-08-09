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
    /// A SQL Server reached **through DuckDB's `mssql` extension** rather than
    /// through `tiberius` — which is what makes it a different kind rather than an
    /// option on [`SourceKind::MsSql`].
    ///
    /// It exists for one reason: a Fabric SQL analytics endpoint answers the first
    /// login with a routing token that `tiberius` cannot follow, so the ordinary
    /// SQL Server source cannot connect to one at all. This path speaks its own
    /// TDS and takes the same Entra sign-in.
    ///
    /// **The dialect is still T-SQL.** The extension's `mssql_scan` runs a query
    /// verbatim on the server, so `top`, `sys.*` and window functions all arrive
    /// as written — this is not a DuckDB source wearing a SQL Server label.
    Fabric,
    MySql,
    /// A MongoDB deployment. Reached like a server — host, port, a login — and
    /// then queried in **DuckDB SQL**: alkyon reads the documents and DuckDB
    /// answers, because MongoDB has no SQL of its own outside Atlas Data
    /// Federation and translating into aggregation pipelines would be a promise
    /// this project does not make.
    Mongo,
    /// A folder of data files on the machine running alkyon, read by DuckDB. It
    /// has a `path` instead of a host, and no credential.
    Folder,
    /// One data file. The same reader as [`SourceKind::Folder`], but the sandbox
    /// is granted that file alone rather than the directory around it, and the
    /// format options describe *this* file rather than a folder's worth.
    File,
    /// A folder in Azure storage — a blob container, an ADLS Gen2 filesystem, or
    /// a Fabric OneLake workspace, which are one API under three names. Reached
    /// over the network like a server, then read like a folder: the files are
    /// mirrored locally, because the sandboxed DuckDB cannot fetch anything
    /// itself.
    Adls,
}

impl SourceKind {
    /// Whether this kind is reached over the network. The false case is what
    /// makes `host`, `port`, encryption and authentication meaningless.
    pub fn is_server(self) -> bool {
        !matches!(
            self,
            SourceKind::Folder | SourceKind::File | SourceKind::Adls
        )
    }

    /// Whether this kind is read as data files by DuckDB rather than queried on
    /// a server. True of Azure storage as well: its files are mirrored locally
    /// and then read exactly as a folder's are.
    pub fn is_files(self) -> bool {
        !self.is_server()
    }

    /// Whether the source's `path` names somewhere on this machine.
    ///
    /// The distinction [`is_files`] cannot make: an Azure source's path is a
    /// container and a folder inside it, so resolving it against the filesystem
    /// — or against the open project — would be answering a different question.
    pub fn is_local_files(self) -> bool {
        matches!(self, SourceKind::Folder | SourceKind::File)
    }

    /// Whether a host is required. Azure storage is not a "server" — there is no
    /// port and nothing to encrypt a choice about — but it is certainly remote.
    pub fn needs_host(self) -> bool {
        self.is_server() || self == SourceKind::Adls
    }

    pub fn default_port(self) -> u16 {
        match self {
            SourceKind::Postgres => 5432,
            SourceKind::MsSql | SourceKind::Fabric => 1433,
            SourceKind::MySql => 3306,
            SourceKind::Mongo => 27017,
            // Not a port at all. Reported as 0 and hidden by the UI, rather than
            // making every summary carry an `Option` for one kind's sake.
            SourceKind::Folder | SourceKind::File | SourceKind::Adls => 0,
        }
    }

    pub fn default_schema(self) -> &'static str {
        match self {
            SourceKind::Postgres => "public",
            SourceKind::MsSql | SourceKind::Fabric => "dbo",
            // MySQL has no schema layer: a schema *is* a database. There is
            // therefore no default to give, and the connector reads an empty
            // schema as "the database this call names". MongoDB is the same shape:
            // a database holds collections and there is no level between them.
            SourceKind::MySql | SourceKind::Mongo => "",
            // Every table a folder or file source exposes. `public` rather than
            // DuckDB's own `main`, because that is the name a SQL user expects.
            SourceKind::Folder | SourceKind::File | SourceKind::Adls => "public",
        }
    }

    /// Which Entra resource a token for this kind has to be minted for.
    ///
    /// Tokens are per-resource: one issued for Azure SQL is refused by storage.
    /// `None` is every kind Entra has nothing to say about — a local folder, or
    /// a PostgreSQL server that is not Azure's.
    pub fn entra_resource(self) -> Option<crate::azure::entra::Resource> {
        match self {
            SourceKind::MsSql | SourceKind::Fabric => Some(crate::azure::entra::Resource::AzureSql),
            SourceKind::Adls => Some(crate::azure::entra::Resource::Storage),
            _ => None,
        }
    }

    /// Where to connect when no database was given. Every engine has one that
    /// always exists and that every login can reach.
    pub fn default_database(self) -> &'static str {
        match self {
            SourceKind::Postgres => "postgres",
            SourceKind::MsSql | SourceKind::Fabric => "master",
            // Readable by everyone and always present, and privilege-filtered by
            // the server so it shows only what this login may see.
            SourceKind::MySql => "information_schema",
            // The one database every deployment has and every login can reach.
            // Not where anyone's data is, which is why the dialogue says so.
            SourceKind::Mongo => "admin",
            // What DuckDB itself calls an in-memory catalogue. Only the fallback
            // for a path with no name of its own — see [`catalogue_name`].
            SourceKind::Folder | SourceKind::File | SourceKind::Adls => "memory",
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
    /// A Delta table: a **directory** of parquet plus a `_delta_log` saying which
    /// of those files are live and what the columns are. Unlike every other
    /// format it is not a file, which is why it has no extension and is found by
    /// the log rather than by a name.
    Delta,
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
            // None: a Delta table is a directory, and `_delta_log` is what says
            // so. Nothing is ever read as Delta because of its name.
            FileFormat::Delta => &[],
        }
    }

    pub const ALL: &'static [FileFormat] = &[
        FileFormat::Csv,
        FileFormat::Parquet,
        FileFormat::Json,
        FileFormat::JsonLines,
        FileFormat::Excel,
        FileFormat::Delta,
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

    /// Whether one table is a directory rather than a file or a group of them.
    pub fn is_delta(self) -> bool {
        matches!(self, FileFormat::Delta)
    }

    /// The DuckDB extension this format needs installed, if any. `parquet` and
    /// `json` are linked in; `delta` has to be fetched once.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            FileFormat::Delta => Some("delta"),
            _ => None,
        }
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
    ///
    /// Also what every other Entra method turns into by the time a connector
    /// sees it: a bearer token is a bearer token, however it was obtained.
    AadToken { token: String },
    /// Signed in through the browser. What is kept is the *refresh* token, and an
    /// access token is minted from it when a connection opens — an access token
    /// alone would make the source stop working an hour after registering it.
    Entra {
        #[serde(default = "default_tenant")]
        tenant: String,
        #[serde(default = "default_client_id")]
        client_id: String,
        /// A just-finished sign-in, quoted by the dialogue. Redeemed server-side
        /// into `refresh_token`, which is why the page never holds one.
        #[serde(default)]
        ticket: Option<String>,
        /// Filled in when the ticket is redeemed or the vault is read. Never
        /// accepted from the wire — `skip` is what stops a request supplying one.
        #[serde(skip)]
        refresh_token: Option<String>,
        /// Who signed in. Display only.
        #[serde(skip)]
        account: String,
    },
    /// Nothing to authenticate: a folder or file source is reached through the
    /// filesystem, with whatever rights the alkyon process already has.
    None,
}

fn default_tenant() -> String {
    crate::azure::entra::DEFAULT_TENANT.to_owned()
}

fn default_client_id() -> String {
    crate::azure::entra::AZURE_CLI_CLIENT_ID.to_owned()
}

impl AuthConfig {
    pub fn method(&self) -> &'static str {
        match self {
            AuthConfig::Password { .. } => "password",
            AuthConfig::Integrated => "integrated",
            AuthConfig::AadToken { .. } => "aad_token",
            AuthConfig::Entra { .. } => "entra",
            AuthConfig::None => "none",
        }
    }

    /// The sign-in this credential is waiting to be given, if any.
    pub fn ticket(&self) -> Option<&str> {
        match self {
            AuthConfig::Entra { ticket, .. } => ticket.as_deref(),
            _ => None,
        }
    }

    /// Put a redeemed sign-in — or one read back out of the vault — in place.
    pub fn with_entra_tokens(self, token: Option<String>, signed_in_as: String) -> AuthConfig {
        match self {
            AuthConfig::Entra {
                tenant, client_id, ..
            } => AuthConfig::Entra {
                tenant,
                client_id,
                ticket: None,
                refresh_token: token,
                account: signed_in_as,
            },
            other => other,
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
            AuthConfig::Entra {
                tenant,
                client_id,
                account,
                ..
            } => f
                .debug_struct("Entra")
                .field("tenant", tenant)
                .field("client_id", client_id)
                .field("account", account)
                .field("refresh_token", &"<redacted>")
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
    /// The tenant and application the sign-in was made against, so renewing it
    /// asks the same place, and the account it was made as, to show in the UI.
    /// The refresh token itself is the secret, and lives in the keychain.
    Entra {
        tenant: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account: String,
    },
    None,
}

impl AuthKind {
    pub fn method(&self) -> &'static str {
        match self {
            AuthKind::Password { .. } => "password",
            AuthKind::Integrated => "integrated",
            AuthKind::AadToken => "aad_token",
            AuthKind::Entra { .. } => "entra",
            AuthKind::None => "none",
        }
    }

    /// Whether using this source needs a secret from the vault.
    pub fn needs_secret(&self) -> bool {
        !matches!(self, AuthKind::Integrated | AuthKind::None)
    }

    /// The account a signed-in source is signed in as, for the sources pane.
    pub fn account(&self) -> Option<&str> {
        match self {
            AuthKind::Entra { account, .. } if !account.is_empty() => Some(account),
            _ => None,
        }
    }

    /// The login name. **Not** a secret — the password is — and the dialogue needs
    /// it to reopen on an existing source without wiping it.
    pub fn username(&self) -> Option<&str> {
        match self {
            AuthKind::Password { username } => Some(username),
            _ => None,
        }
    }

    /// The Entra tenant and application this source signs in through, when they
    /// are not the defaults. Same reason: an edit that could not see them would
    /// silently reset them.
    pub fn entra(&self) -> Option<(&str, &str)> {
        match self {
            AuthKind::Entra {
                tenant, client_id, ..
            } => Some((tenant, client_id)),
            _ => None,
        }
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
            account: self.auth.account().map(str::to_owned),
            // Everything the dialogue needs to reopen on this source without
            // losing something. None of it is a credential.
            username: self.auth.username().map(str::to_owned),
            tenant: self.auth.entra().map(|(tenant, _)| tenant.to_owned()),
            client_id: self.auth.entra().map(|(_, client)| client.to_owned()),
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
            (
                AuthKind::Entra {
                    tenant,
                    client_id,
                    account,
                },
                Some(refresh_token),
            ) => AuthConfig::Entra {
                tenant: tenant.clone(),
                client_id: client_id.clone(),
                ticket: None,
                refresh_token: Some(refresh_token),
                account: account.clone(),
            },
            (AuthKind::Integrated, _) => AuthConfig::Integrated,
            (AuthKind::None, _) => AuthConfig::None,
            (AuthKind::Password { .. } | AuthKind::AadToken | AuthKind::Entra { .. }, None) => {
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
            AuthConfig::Entra {
                tenant,
                client_id,
                refresh_token,
                account,
                ..
            } => (
                AuthKind::Entra {
                    tenant,
                    client_id,
                    account,
                },
                refresh_token,
            ),
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
    /// Who a signed-in source is signed in as. Absent for every other method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The login name of a password source. Not a secret; the password is, and it
    /// never leaves the vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The Entra tenant and application a signed-in source uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
