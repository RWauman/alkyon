use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::connectors::files::FilesConnector;
use crate::connectors::mssql::MssqlConnector;
use crate::connectors::mysql::MySqlConnector;
use crate::connectors::postgres::PgConnector;
use crate::connectors::{Connection, Connector};
use crate::error::{Error, Result};
use crate::model::{Dialect, Scope, SourceConfig, SourceKind, SourceRecord, SourceSummary};
use crate::schema::SchemaCache;
use crate::vault::Vault;

const SOURCES_FILE: &str = "sources.json";
const WORKSPACE_FILE: &str = "workspace.json";
/// Project sources live here, relative to the open folder. Committable — it holds
/// hosts and usernames, never a credential.
const PROJECT_SOURCES: &str = ".alkyon/sources.json";

/// Read a `sources.json`, tagging every record with the scope it came from.
/// A missing file is an empty list; a corrupt one is reported and skipped, because
/// refusing to start over a stray comma would be worse.
fn read_sources(file: &Path, scope: Scope) -> Vec<SourceRecord> {
    let raw = match std::fs::read_to_string(file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(path = %file.display(), error = %e, "cannot read sources");
            return Vec::new();
        }
    };
    match serde_json::from_str::<Vec<SourceRecord>>(&raw) {
        Ok(records) => {
            tracing::info!(count = records.len(), path = %file.display(), scope = scope.prefix(), "loaded sources");
            records
                .into_iter()
                .map(|mut record| {
                    record.scope = scope;
                    record
                })
                .collect()
        }
        Err(e) => {
            tracing::error!(path = %file.display(), error = %e, "sources file is not valid JSON, ignoring it");
            Vec::new()
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredWorkspace {
    root: PathBuf,
}

/// Where `sources.json` lives: `ALKYON_CONFIG_DIR` if set, otherwise the
/// platform config directory (`%APPDATA%\alkyon`, `~/.config/alkyon`,
/// `~/Library/Application Support/alkyon`).
///
/// The override is what makes a container or a portable install work: the
/// directory next to an installed `.exe` is usually not writable.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ALKYON_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "alkyon").map(|dirs| dirs.config_dir().to_path_buf())
}

/// The registry of sources.
///
/// Records — hosts, ports, usernames — are written to `sources.json`. Secrets go
/// to the [`Vault`] and are read back only for the moment a connection is
/// opened, so no credential is ever held in memory between requests.
pub struct AppState {
    sources: RwLock<BTreeMap<String, SourceRecord>>,
    vault: Vault,
    /// `None` disables persistence, which is what the tests use.
    file: Option<PathBuf>,
    /// The open folder: the `.sql` tree, the file API and the terminal's working
    /// directory all hang off this.
    workspace: RwLock<Option<PathBuf>>,
    /// Where the workspace root is remembered between runs.
    workspace_file: Option<PathBuf>,
    /// A shell over HTTP is a different risk class from a query endpoint, so it
    /// is off unless the server is bound to loopback. See [`terminal_allowed`].
    pub terminal_enabled: bool,
    /// Schema snapshots, feeding both autocompletion and the search bar.
    schema: SchemaCache,
    postgres: PgConnector,
    mssql: MssqlConnector,
    mysql: MySqlConnector,
    files: FilesConnector,
}

/// Whether to expose `/ws/terminal`. Alkyon has no authentication by design, so
/// on a non-loopback bind the terminal would be an unauthenticated remote shell.
/// `ALKYON_TERMINAL=always` is the deliberate opt-in for a trusted network.
pub fn terminal_allowed(bind: &std::net::SocketAddr) -> bool {
    if std::env::var("ALKYON_TERMINAL").as_deref() == Ok("always") {
        tracing::warn!("ALKYON_TERMINAL=always: the terminal is reachable without authentication");
        return true;
    }
    bind.ip().is_loopback()
}

impl AppState {
    /// An ephemeral registry: memory vault, nothing written to disk.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sources: RwLock::new(BTreeMap::new()),
            vault: Vault::memory(),
            file: None,
            workspace: RwLock::new(None),
            workspace_file: None,
            terminal_enabled: true,
            schema: SchemaCache::default(),
            postgres: PgConnector::default(),
            mssql: MssqlConnector,
            mysql: MySqlConnector::default(),
            files: FilesConnector,
        })
    }

    /// Load the user registry, plus the project one if a folder reopens.
    pub fn load(vault: Vault, dir: &Path, terminal_enabled: bool) -> Result<Arc<Self>> {
        let file = dir.join(SOURCES_FILE);

        // A folder that has since been deleted or unmounted should not stop the
        // workbench from starting; it just comes back closed.
        let workspace_file = dir.join(WORKSPACE_FILE);
        let workspace = std::fs::read_to_string(&workspace_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<StoredWorkspace>(&raw).ok())
            .map(|stored| stored.root)
            .filter(|root| {
                if root.is_dir() {
                    tracing::info!(root = %root.display(), "reopened workspace");
                    true
                } else {
                    tracing::warn!(root = %root.display(), "workspace is gone, starting closed");
                    false
                }
            });

        let mut sources = BTreeMap::new();
        for record in read_sources(&file, Scope::User) {
            sources.insert(record.key(), record);
        }
        if let Some(root) = &workspace {
            for record in read_sources(&root.join(PROJECT_SOURCES), Scope::Project) {
                sources.insert(record.key(), record);
            }
        }

        Ok(Arc::new(Self {
            sources: RwLock::new(sources),
            vault,
            file: Some(file),
            workspace: RwLock::new(workspace),
            workspace_file: Some(workspace_file),
            terminal_enabled,
            schema: SchemaCache::default(),
            postgres: PgConnector::default(),
            mssql: MssqlConnector,
            mysql: MySqlConnector::default(),
            files: FilesConnector,
        }))
    }

    /// The keychain entry for a record.
    ///
    /// Project keys carry the folder path: two unrelated projects may each define
    /// a `warehouse`, and they must not end up sharing one credential. The
    /// *registry* key does not need the path, because only one folder is open at a
    /// time.
    async fn vault_key(&self, record: &SourceRecord) -> String {
        match record.scope {
            Scope::User => format!("user:{}", record.id),
            Scope::Project => {
                let root = self
                    .workspace()
                    .await
                    .map(|root| root.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("project:{root}:{}", record.id)
            }
        }
    }

    /// Fetch a credential, moving it to its scoped key if it predates scopes.
    ///
    /// Before scopes existed the account name was the bare id. Without this an
    /// upgrade would silently orphan every stored credential and every source
    /// would come back unreachable.
    async fn load_secret(&self, record: &SourceRecord) -> Result<Option<String>> {
        let key = self.vault_key(record).await;
        if let Some(secret) = self.vault.load(&key)? {
            return Ok(Some(secret));
        }

        // Only user sources can have a pre-scope entry; project ones are new.
        if record.scope == Scope::User {
            if let Some(secret) = self.vault.load(&record.id)? {
                tracing::info!(id = %record.id, "moving credential to its scoped key");
                self.vault.store(&key, &secret)?;
                self.vault.delete(&record.id)?;
                return Ok(Some(secret));
            }
        }
        Ok(None)
    }

    /// Swap the project sources for those of `root`, or drop them when it is
    /// `None`. Called whenever the open folder changes.
    async fn reload_project_sources(&self, root: Option<&Path>) {
        let mut sources = self.sources.write().await;
        sources.retain(|_, record| record.scope != Scope::Project);
        if let Some(root) = root {
            for record in read_sources(&root.join(PROJECT_SOURCES), Scope::Project) {
                sources.insert(record.key(), record);
            }
        }
    }

    pub async fn workspace(&self) -> Option<PathBuf> {
        self.workspace.read().await.clone()
    }

    /// The workspace root, or a clear error when nothing is open — every file
    /// route needs the same sentence.
    pub async fn workspace_root(&self) -> Result<PathBuf> {
        self.workspace()
            .await
            .ok_or_else(|| Error::BadRequest("no folder is open".into()))
    }

    pub async fn open_workspace(&self, path: &str) -> Result<PathBuf> {
        let root = crate::workspace::open(path)?;
        *self.workspace.write().await = Some(root.clone());
        self.persist_workspace(Some(&root))?;
        // The project registry belongs to the folder, so it follows it.
        self.reload_project_sources(Some(&root)).await;
        tracing::info!(root = %root.display(), "opened workspace");
        Ok(root)
    }

    pub async fn close_workspace(&self) -> Result<()> {
        *self.workspace.write().await = None;
        self.reload_project_sources(None).await;
        self.persist_workspace(None)
    }

    fn persist_workspace(&self, root: Option<&Path>) -> Result<()> {
        let Some(file) = &self.workspace_file else {
            return Ok(());
        };
        match root {
            Some(root) => {
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let stored = StoredWorkspace {
                    root: root.to_path_buf(),
                };
                std::fs::write(file, serde_json::to_string_pretty(&stored)?)?;
            }
            None => match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    pub fn schema(&self) -> &SchemaCache {
        &self.schema
    }

    /// The cached snapshot for `key`/`database`, loading it if absent.
    ///
    /// One round trip per snapshot, so this is cheap enough to do the moment a
    /// source is selected — which is what makes autocompletion complete straight
    /// away instead of filling in as you browse.
    pub async fn snapshot(
        &self,
        key: &str,
        database: Option<&str>,
        refresh: bool,
    ) -> Result<Arc<crate::schema::Snapshot>> {
        let record = self.record(key).await?;
        let database = database
            .map(str::to_owned)
            .unwrap_or_else(|| record.database().to_owned());

        if !refresh {
            if let Some(cached) = self.schema.get(&record.key(), &database).await {
                return Ok(cached);
            }
        }

        let connection = self.open(key, Some(&database)).await?;
        let tables = connection.snapshot(&database).await?;
        tracing::info!(
            source = %record.key(),
            database = %database,
            tables = tables.len(),
            "loaded schema snapshot"
        );
        Ok(self
            .schema
            .put(crate::schema::Snapshot {
                source: record.key(),
                database,
                tables,
            })
            .await)
    }

    pub fn connector(&self, kind: SourceKind) -> &dyn Connector {
        match kind {
            SourceKind::Postgres => &self.postgres,
            SourceKind::MsSql => &self.mssql,
            SourceKind::MySql => &self.mysql,
            SourceKind::Files => &self.files,
        }
    }

    pub fn dialect(&self, kind: SourceKind) -> Dialect {
        self.connector(kind).dialect()
    }

    pub async fn summaries(&self) -> Vec<SourceSummary> {
        self.sources
            .read()
            .await
            .values()
            .map(|record| record.summary(self.dialect(record.kind)))
            .collect()
    }

    /// `key` is `user:<id>` or `project:<id>`. A bare `<id>` is accepted too when
    /// exactly one scope defines it, so URLs and `SWAP` stay pleasant to type.
    pub async fn record(&self, key: &str) -> Result<SourceRecord> {
        let sources = self.sources.read().await;
        if let Some(record) = sources.get(key) {
            return Ok(record.clone());
        }

        let mut matches = sources.values().filter(|record| record.id == key);
        match (matches.next(), matches.next()) {
            (Some(record), None) => Ok(record.clone()),
            (Some(_), Some(_)) => Err(Error::BadRequest(format!(
                "`{key}` is defined in both scopes — say `user:{key}` or `project:{key}`"
            ))),
            _ => Err(Error::UnknownSource(key.to_owned())),
        }
    }

    /// Store the credential, remember the record, write the file.
    pub async fn register(&self, config: SourceConfig) -> Result<SourceSummary> {
        let (record, secret) = config.split();
        if record.scope == Scope::Project && self.workspace().await.is_none() {
            return Err(Error::BadRequest(
                "a project source needs an open folder to live in".into(),
            ));
        }

        let key = record.key();
        let vault_key = self.vault_key(&record).await;
        let mut sources = self.sources.write().await;
        if sources.contains_key(&key) {
            return Err(Error::DuplicateSource(key));
        }

        if let Some(secret) = secret {
            self.vault.store(&vault_key, &secret)?;
        }
        let summary = record.summary(self.dialect(record.kind));
        sources.insert(key, record);
        self.persist(&sources).await?;
        Ok(summary)
    }

    pub async fn remove(&self, key: &str) -> Result<()> {
        // Resolve first, so a bare id removes the right one — and errors the same
        // way `record` does when it is ambiguous.
        let record = self.record(key).await?;
        let vault_key = self.vault_key(&record).await;

        // A cached schema must not outlive the source it describes.
        self.schema.forget(&record.key()).await;

        let mut sources = self.sources.write().await;
        sources.remove(&record.key());
        self.vault.delete(&vault_key)?;
        self.persist(&sources).await
    }

    /// Open a connection to `id`, bound to `database` if given and to the
    /// source's own default otherwise. The secret is fetched here and dropped
    /// when the connection is built.
    pub async fn open(&self, key: &str, database: Option<&str>) -> Result<Box<dyn Connection>> {
        let record = self.record(key).await?;
        let secret = if record.auth.needs_secret() {
            self.load_secret(&record).await?
        } else {
            None
        };

        let mut config = record.with_secret(secret)?;
        if let Some(db) = database {
            config.database = Some(db.to_owned());
        }
        self.connector(config.kind).connect(&config).await
    }

    /// Bulk import from a credentials file, overwriting sources of the same id.
    ///
    /// Nothing connects: a server being down should not stop the workbench from
    /// starting. With the keychain vault this is a one-shot import — the secrets
    /// land in the keychain and the file can be deleted.
    pub async fn import(&self, configs: Vec<SourceConfig>) -> Result<()> {
        let mut sources = self.sources.write().await;
        for config in configs {
            let (record, secret) = config.split();
            if let Some(secret) = secret {
                self.vault.store(&self.vault_key(&record).await, &secret)?;
            }
            tracing::info!(key = %record.key(), kind = ?record.kind, host = %record.host, "imported source");
            sources.insert(record.key(), record);
        }
        self.persist(&sources).await
    }

    /// Write each scope back to its own file, through a temporary so an
    /// interrupted write cannot leave half a registry behind.
    async fn persist(&self, sources: &BTreeMap<String, SourceRecord>) -> Result<()> {
        let Some(user_file) = &self.file else {
            return Ok(());
        };

        write_scope(user_file, sources, Scope::User)?;

        // Project records only have somewhere to go while a folder is open. When
        // none is, there are none in the map either.
        if let Some(root) = self.workspace().await {
            write_scope(&root.join(PROJECT_SOURCES), sources, Scope::Project)?;
        }
        Ok(())
    }
}

fn write_scope(file: &Path, sources: &BTreeMap<String, SourceRecord>, scope: Scope) -> Result<()> {
    let records: Vec<&SourceRecord> = sources
        .values()
        .filter(|record| record.scope == scope)
        .collect();

    // Do not create `.alkyon/` in a project that has no project sources.
    if records.is_empty() && scope == Scope::Project && !file.exists() {
        return Ok(());
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(&records)?)?;
    std::fs::rename(&temporary, file)?;
    Ok(())
}
