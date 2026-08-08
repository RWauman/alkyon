//! MongoDB as a source, queried in **DuckDB SQL**.
//!
//! MongoDB has no SQL of its own — `$sql` exists, but only inside Atlas Data
//! Federation, which is a separate paid service and unreachable from a
//! self-hosted deployment. So rather than translate SQL into aggregation
//! pipelines and be wrong in the interesting cases, alkyon does what it already
//! does for a spreadsheet: it reads the documents itself and lets DuckDB answer
//! the SQL.
//!
//! Each collection becomes a view over the documents, as **NDJSON**, which is the
//! part that makes this worth doing:
//!
//! ```sql
//! select address.city, unnest(tags) as tag, count(*)
//! from customer group by all
//! ```
//!
//! Nesting survives, because DuckDB's JSON reader infers structs and lists rather
//! than being handed a flattened table. A field absent from some documents is
//! `NULL` on those rows, and a field holding four different types across a
//! collection lands as JSON instead of as a lie.
//!
//! **What it costs: documents arrive before they are filtered.** There is no
//! pushdown — a `where` clause narrows rows that have already been read. The
//! defence is a cap that fails loudly rather than truncating, and `@import` for
//! the times the server should do the work:
//!
//! ```text
//! -- @import big = mongo-dev/alkyon_demo : [{"$match": {…}}, {"$group": {…}}]
//! ```

use std::path::PathBuf;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::TryStreamExt;
use mongodb::bson::{Bson, Document};
use mongodb::options::ClientOptions;
use mongodb::Client;

use super::{Connection, Connector};
use crate::error::{Error, Result};
use crate::federation::{self, open_duckdb_in, quote_identifier, quote_literal, Sandbox};
use crate::model::{
    ColumnInfo, Dialect, RowBatch, SourceConfig, TableInfo, TableKind, TableSchema, TlsMode,
};

/// How many documents are read to work out a collection's columns.
///
/// A collection has no schema, so this is a sample and nothing more: a field that
/// appears only in the ten-thousandth document is not in it. Said plainly in the
/// guide rather than pretended away.
const SAMPLE: usize = 200;

/// Databases MongoDB keeps for itself. Listed last rather than hidden — someone
/// looking for `system.users` should find it — but they are not what anyone came
/// for.
const SYSTEM_DATABASES: &[&str] = &["admin", "config", "local"];

/// How many documents one collection may bring back before the query is refused.
///
/// A cap rather than a truncation, for the reason the import cap gives: a join
/// quietly missing half its rows is worse than a query that failed. Raised with
/// `ALKYON_MONGO_MAX_DOCS`.
fn max_documents() -> usize {
    std::env::var("ALKYON_MONGO_MAX_DOCS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200_000)
}

pub struct MongoConnector;

/// Build the connection string the driver wants.
///
/// Assembled rather than taken whole so that a source is described by the same
/// fields as every other one — host, port, credential, encryption — and so that a
/// password never has to be percent-encoded by hand.
fn client_options(cfg: &SourceConfig) -> Result<ClientOptions> {
    use mongodb::options::{Credential, ServerAddress, Tls, TlsOptions};

    let mut options = ClientOptions::builder()
        .hosts(vec![ServerAddress::Tcp {
            host: cfg.host.clone(),
            port: Some(cfg.port()),
        }])
        .app_name(Some("alkyon".to_owned()))
        .build();

    options.credential = match &cfg.auth {
        crate::model::AuthConfig::Password { username, password } => Some(
            Credential::builder()
                .username(username.clone())
                .password(password.clone())
                // The database the *credential* lives in, which is `admin` for a
                // root user and is not the database being queried.
                .source(Some("admin".to_owned()))
                .build(),
        ),
        // Nothing to authenticate: a mongod started without `--auth`.
        crate::model::AuthConfig::None => None,
        other => {
            return Err(Error::Unsupported(format!(
                "MongoDB takes a login and password; `{}` is not something it accepts",
                other.method()
            )))
        }
    };

    options.tls = match cfg.tls {
        TlsMode::Disable => None,
        TlsMode::Prefer | TlsMode::TrustCertificate => Some(Tls::Enabled(
            TlsOptions::builder()
                .allow_invalid_certificates(true)
                .build(),
        )),
        TlsMode::Require => Some(Tls::Enabled(TlsOptions::builder().build())),
    };

    Ok(options)
}

#[async_trait]
impl Connector for MongoConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let client = Client::with_options(client_options(config)?)
            .map_err(|e| Error::Mongo(format!("cannot reach the deployment: {e}")))?;

        // Prove the credentials before the source is registered, the way every
        // other connector does — building a client on its own connects to
        // nothing.
        client
            .database("admin")
            .run_command(mongodb::bson::doc! { "ping": 1 })
            .await
            .map_err(explain)?;

        Ok(Box::new(MongoConnection {
            client,
            database: config.database().to_owned(),
        }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
}

/// Turn the driver's error into something worth reading.
///
/// [`Error::Mongo`] and not `BadRequest`: this is what the *server* said, so it
/// belongs in the same class as what PostgreSQL and SQL Server say — a refused
/// login is a bad gateway, not a malformed request.
fn explain(error: mongodb::error::Error) -> Error {
    let said = error.to_string();
    // The one everybody hits: a right password against the wrong auth database,
    // or no `--auth` at all on the server.
    if said.contains("Authentication failed") {
        return Error::Mongo(
            "the login was refused. The credential is checked against `admin`, which is \
             where a root user lives; a user created inside another database has to be \
             given there."
                .into(),
        );
    }
    Error::Mongo(said)
}

pub struct MongoConnection {
    client: Client,
    /// The database the source points at, and the one a bare table name means.
    database: String,
}

/// The documents of one collection, written where DuckDB can read them.
///
/// A file rather than a table built cell by cell: DuckDB's JSON reader is what
/// turns `{"address": {"city": …}}` into a struct, and handing it flattened rows
/// would throw away the nesting that makes a document worth storing.
struct Ndjson {
    /// The collection, and therefore the view.
    name: String,
    file: PathBuf,
}

/// The directory the NDJSON lives in, removed when the query is done.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("alkyon-mongo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a leftover directory in the temp folder is untidy, not
        // wrong, and there is nothing useful to do with the error here.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl MongoConnection {
    fn database_of(&self, db: &str) -> mongodb::Database {
        self.client.database(self.name_of(db))
    }

    /// The database a request means: the one it names, or the source's own.
    fn name_of<'a>(&'a self, db: &'a str) -> &'a str {
        if db.is_empty() {
            &self.database
        } else {
            db
        }
    }

    /// The schema the collections of `db` land in on the DuckDB side, which is
    /// **the database's own name**.
    ///
    /// Not a fixed `public`: the explorer qualifies a table with the schema it was
    /// listed under, so a view created under any other name is a table that cannot
    /// be clicked —
    ///
    /// ```text
    /// SELECT * FROM "alkyon_demo"."order_line"
    ///   → schema "alkyon_demo" does not exist
    /// ```
    ///
    /// The database is the schema here as it is for MySQL, and one string has to
    /// serve both halves or they drift apart.
    fn schema_of(&self, db: &str) -> String {
        self.name_of(db).to_owned()
    }

    /// The collections and views in `db`.
    async fn collections(&self, db: &str) -> Result<Vec<TableInfo>> {
        let database = self.database_of(db);
        let schema = self.schema_of(db);
        let mut cursor = database.list_collections().await.map_err(explain)?;

        let mut out = Vec::new();
        while let Some(spec) = cursor.try_next().await.map_err(explain)? {
            // `system.*` is MongoDB's own bookkeeping and not a table anyone means.
            if spec.name.starts_with("system.") {
                continue;
            }
            out.push(TableInfo {
                // The database is the schema, as with MySQL — and the resolved name
                // rather than the one asked for, so that the qualified name the
                // explorer inserts is one [`MongoConnection::session`] has built.
                schema: schema.clone(),
                name: spec.name,
                kind: match spec.collection_type {
                    mongodb::results::CollectionType::View => TableKind::View,
                    _ => TableKind::Table,
                },
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Read a collection into NDJSON, refusing rather than truncating.
    async fn dump(
        &self,
        scratch: &Scratch,
        db: &str,
        collection: &str,
        limit: Option<usize>,
    ) -> Result<Ndjson> {
        use std::io::Write;

        let cap = limit.unwrap_or_else(max_documents);
        let mut cursor = self
            .database_of(db)
            .collection::<Document>(collection)
            .find(Document::new())
            .await
            .map_err(explain)?;

        let file = scratch.0.join(format!("{}.ndjson", sanitise(collection)));
        let mut sink = std::io::BufWriter::new(std::fs::File::create(&file)?);
        let mut written = 0usize;

        while let Some(document) = cursor.try_next().await.map_err(explain)? {
            if written >= cap {
                // Only a sample was asked for: stopping is the whole point.
                if limit.is_some() {
                    break;
                }
                return Err(Error::BadRequest(format!(
                    "`{collection}` has more than {cap} documents, and alkyon reads a \
                     collection before it can query it. Narrow it with an `@import` \
                     pipeline, or raise ALKYON_MONGO_MAX_DOCS."
                )));
            }
            let value = json_of(&Bson::Document(document));
            serde_json::to_writer(&mut sink, &value)?;
            sink.write_all(b"\n")?;
            written += 1;
        }
        sink.flush()?;

        tracing::debug!(collection, documents = written, "read a MongoDB collection");
        Ok(Ndjson {
            name: collection.to_owned(),
            file,
        })
    }

    /// A DuckDB session with one view per dumped collection, in `schema`.
    ///
    /// `schema` is on the search path, so a bare `order_line` and a qualified
    /// `"alkyon_demo"."order_line"` both find the same view.
    fn session(&self, schema: &str, dumped: &[Ndjson]) -> Result<duckdb::Connection> {
        let sandbox = Sandbox {
            directories: Vec::new(),
            // Each file by name: the session is granted the documents of this
            // query and nothing else on the machine.
            files: dumped.iter().map(|d| d.file.clone()).collect(),
        };
        let connection = open_duckdb_in(&sandbox, Some(schema))?;

        for dump in dumped {
            let address = dump.file.to_string_lossy().replace('\\', "/");
            connection
                .execute_batch(&format!(
                    "CREATE OR REPLACE VIEW {}.{} AS SELECT * FROM read_json({});",
                    quote_identifier(schema),
                    quote_identifier(&dump.name),
                    quote_literal(&address),
                ))
                .map_err(|e| {
                    Error::Federated(format!("cannot read `{}` as JSON: {e}", dump.name))
                })?;
        }
        Ok(connection)
    }
}

/// A collection name as a filename. Remote names are not ours to trust, and a
/// collection may legitimately hold a dot or a dollar.
fn sanitise(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// BSON as JSON a person would want to read.
///
/// Deliberately not MongoDB's extended JSON: `{"$oid": "…"}` is faithful and
/// useless in a grid. What matters here is that a value arrives as something SQL
/// can work with, and that nothing is silently made less exact:
///
/// - an **ObjectId** is its hexadecimal string, which is how everyone writes one
/// - a **Decimal128** is a *string*, not a number. It is money — turning it into a
///   float to make the column numeric is exactly the trade this codebase refuses
///   elsewhere. Cast it: `cast(credit as decimal(18,2))`
/// - a **date** is RFC 3339, which DuckDB reads as a timestamp
/// - **binary** is base64, because there is nothing better to do with it
pub(crate) fn json_of(value: &Bson) -> serde_json::Value {
    use serde_json::Value;

    match value {
        Bson::Double(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Bson::String(s) => Value::String(s.clone()),
        Bson::Array(items) => Value::Array(items.iter().map(json_of).collect()),
        Bson::Document(document) => Value::Object(
            document
                .iter()
                .map(|(key, value)| (key.clone(), json_of(value)))
                .collect(),
        ),
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Int32(n) => Value::Number((*n).into()),
        Bson::Int64(n) => Value::Number((*n).into()),
        Bson::ObjectId(id) => Value::String(id.to_hex()),
        Bson::DateTime(when) => Value::String(
            when.try_to_rfc3339_string()
                .unwrap_or_else(|_| when.timestamp_millis().to_string()),
        ),
        Bson::Decimal128(d) => Value::String(d.to_string()),
        Bson::Binary(binary) => Value::String(base64_of(&binary.bytes)),
        Bson::RegularExpression(regex) => {
            Value::String(format!("/{}/{}", regex.pattern, regex.options))
        }
        Bson::JavaScriptCode(code) => Value::String(code.clone()),
        Bson::JavaScriptCodeWithScope(code) => Value::String(code.code.clone()),
        Bson::Symbol(s) => Value::String(s.clone()),
        Bson::Timestamp(t) => Value::String(format!("{}:{}", t.time, t.increment)),
        Bson::DbPointer(_) => Value::String("<dbpointer>".to_owned()),
        // `undefined`, `minKey` and `maxKey` have no value to carry, and null is
        // the only honest thing a column can hold for them.
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => Value::Null,
    }
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The collections whose name appears anywhere in `sql`.
///
/// The same reasoning as a folder source's: reading every collection to answer a
/// query against one would be absurd, and a view can only be referenced if its
/// name occurs in the text. Longest first, so `order_line` is not missed because
/// `order` matched.
pub(crate) fn referenced(sql: &str, collections: &[String]) -> Vec<String> {
    let haystack = sql.to_lowercase();
    let mut named: Vec<String> = collections
        .iter()
        .filter(|name| haystack.contains(&name.to_lowercase()))
        .cloned()
        .collect();
    named.sort_by_key(|name| std::cmp::Reverse(name.len()));
    named
}

#[async_trait]
impl Connection for MongoConnection {
    async fn list_databases(&self) -> Result<Vec<String>> {
        let mut names = self
            .client
            .list_database_names()
            .await
            .map_err(explain)?
            .into_iter()
            .collect::<Vec<_>>();
        names.sort_by_key(|name| {
            (
                SYSTEM_DATABASES.contains(&name.as_str()),
                name.to_lowercase(),
            )
        });
        Ok(names)
    }

    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>> {
        self.collections(db).await
    }

    async fn list_columns(&self, db: &str, _schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let scratch = Scratch::new()?;
        let schema = self.schema_of(db);
        let dumped = self.dump(&scratch, db, table, Some(SAMPLE)).await?;
        let connection = self.session(&schema, std::slice::from_ref(&dumped))?;
        federation::describe_view(&connection, &schema, &dumped.name)
    }

    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>> {
        let collections = self.collections(db).await?;
        let scratch = Scratch::new()?;
        let schema = self.schema_of(db);

        let mut tables = Vec::new();
        for info in collections {
            // One unreadable collection must not cost the schema of the rest.
            let dumped = match self.dump(&scratch, db, &info.name, Some(SAMPLE)).await {
                Ok(dumped) => dumped,
                Err(e) => {
                    tracing::warn!(collection = %info.name, error = %e, "skipped in snapshot");
                    continue;
                }
            };
            let connection = self.session(&schema, std::slice::from_ref(&dumped))?;
            match federation::describe_view(&connection, &schema, &dumped.name) {
                Ok(columns) => tables.push(TableSchema {
                    schema: info.schema,
                    name: info.name,
                    kind: info.kind,
                    columns,
                }),
                Err(e) => tracing::warn!(collection = %dumped.name, error = %e, "not describable"),
            }
        }
        Ok(tables)
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let db = self.database.clone();
            let names: Vec<String> = self
                .collections(&db)
                .await?
                .into_iter()
                .map(|info| info.name)
                .collect();

            let scratch = Scratch::new()?;
            let mut dumped = Vec::new();
            for name in referenced(sql, &names) {
                dumped.push(self.dump(&scratch, &db, &name, None).await?);
            }

            let connection = self.session(&db, &dumped)?;
            let sql = sql.to_owned();
            let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

            // DuckDB is synchronous, so it gets its own thread rather than
            // stalling the runtime for the length of the query.
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                federation::run(&connection, &sql, &sink)
            });

            while let Some(batch) = source.recv().await {
                yield batch?;
            }
            worker
                .await
                .map_err(|e| Error::Federated(format!("the query worker failed: {e}")))??;
            // Held until here on purpose: the documents have to outlive the query
            // that reads them.
            drop(scratch);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{doc, oid::ObjectId, Decimal128};
    use serde_json::json;

    #[test]
    fn a_document_becomes_json_a_person_can_read() {
        let id = ObjectId::parse_str("64b7f9c2e1b2a3d4e5f60718").unwrap();
        let document = doc! {
            "_id": id,
            "name": "Ada",
            "credit": "13.37".parse::<Decimal128>().unwrap(),
            "tags": ["gold", "silver"],
            "address": { "city": "Brussels" },
            "missing": Bson::Null,
        };

        assert_eq!(
            json_of(&Bson::Document(document)),
            json!({
                // Hexadecimal, not `{"$oid": …}`: extended JSON is faithful and
                // unreadable in a grid.
                "_id": "64b7f9c2e1b2a3d4e5f60718",
                "name": "Ada",
                // A string, so no cent is lost on the way to a float.
                "credit": "13.37",
                "tags": ["gold", "silver"],
                "address": { "city": "Brussels" },
                "missing": null,
            })
        );
    }

    #[test]
    fn the_types_with_no_column_to_go_in_still_arrive() {
        assert_eq!(json_of(&Bson::MinKey), json!(null));
        assert_eq!(json_of(&Bson::MaxKey), json!(null));
        assert_eq!(json_of(&Bson::Undefined), json!(null));
        assert_eq!(json_of(&Bson::Int64(7)), json!(7));
        assert_eq!(json_of(&Bson::Boolean(true)), json!(true));
        // Not a number DuckDB could hold, and not a reason to lose the document.
        assert_eq!(json_of(&Bson::Double(f64::NAN)), json!(null));
    }

    /// The bug this guards: `order` matching first and `order_line` never being
    /// read, so a query against it finds no such table.
    #[test]
    fn a_longer_collection_name_is_not_shadowed_by_a_shorter_one() {
        let collections = vec!["order".to_owned(), "order_line".to_owned()];
        assert_eq!(
            referenced("select * from order_line", &collections),
            ["order_line", "order"],
            "both are named in the text, and the longer one comes first"
        );
        assert!(referenced("select 1", &collections).is_empty());
        // Case does not matter: DuckDB folds unquoted identifiers.
        assert_eq!(referenced("SELECT * FROM Order", &collections), ["order"]);
    }

    #[test]
    fn a_collection_name_cannot_escape_the_scratch_directory() {
        // Six separators and dots, six underscores: nothing left that a path
        // walker could read as "go up".
        assert_eq!(sanitise("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitise("order_line"), "order_line");
        // Legitimate names that are not filenames.
        assert_eq!(sanitise("sales.2026"), "sales_2026");
    }
}
