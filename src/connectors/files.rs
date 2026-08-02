//! A folder — or a single file — as a source.
//!
//! It is a [`Connector`] like any other rather than a second kind of registry
//! entry, and that pays for itself twice over: the sources pane, the scopes, the
//! green dot, the explorer tree, autocompletion and the schema search all work
//! unchanged, and `-- @import x = my-folder : select …` federates a folder
//! against a database with no new machinery.
//!
//! Every data file becomes a DuckDB **view**, so `select * from customers` means
//! `customers.parquet`. Subdirectories become schemas, which is what keeps
//! `sales/orders.csv` and `ops/orders.csv` apart.
//!
//! The session is the same confined DuckDB the federator uses: external access
//! off, this path as the only exception, no extension may be installed, and the
//! configuration is frozen before any user SQL runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;

use super::{Connection, Connector};
use crate::error::{Error, Result};
use crate::federation::{self, open_duckdb, quote_identifier, quote_literal, Sandbox};
use crate::model::{
    ColumnInfo, Dialect, RowBatch, SourceConfig, TableInfo, TableKind, TableSchema,
};

/// Extensions alkyon will read, and the DuckDB function that reads each.
///
/// Only formats that are **linked into the binary** — CSV is core, parquet and
/// JSON come from the Cargo features. Anything else would mean DuckDB fetching
/// an extension at runtime, which the sandbox forbids on purpose.
const READERS: &[(&str, &str)] = &[
    ("csv", "read_csv"),
    ("tsv", "read_csv"),
    ("txt", "read_csv"),
    ("parquet", "read_parquet"),
    ("json", "read_json"),
    ("ndjson", "read_json"),
    ("jsonl", "read_json"),
];

/// A folder source is browsed, not crawled. Past this the tree stops being
/// useful and every snapshot would read hundreds of file headers.
const MAX_FILES: usize = 200;
const MAX_DEPTH: usize = 6;

/// DuckDB's default schema, and where files at the root of the source live.
const ROOT_SCHEMA: &str = "main";

pub(crate) fn reader_for(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    READERS
        .iter()
        .find(|(suffix, _)| *suffix == extension)
        .map(|(_, reader)| *reader)
}

pub(crate) fn readable_extensions() -> String {
    let mut seen: Vec<&str> = Vec::new();
    for (suffix, _) in READERS {
        if !seen.contains(suffix) {
            seen.push(suffix);
        }
    }
    seen.join(", ")
}

/// One data file, as both the explorer tree and DuckDB see it.
#[derive(Debug, Clone)]
struct DataFile {
    /// The subdirectory it sits in, `main` at the root.
    schema: String,
    /// The view name — the file stem, or the whole filename if two files in one
    /// directory would otherwise collide.
    name: String,
    path: PathBuf,
    reader: &'static str,
}

impl DataFile {
    /// `read_parquet('C:/data/sales.parquet')`
    ///
    /// Forward slashes because that is what DuckDB normalises paths to when it
    /// checks them against the sandbox, so the literal and the permission are
    /// written the same way.
    fn scan(&self) -> String {
        let path = self.path.to_string_lossy().replace('\\', "/");
        format!("{}({})", self.reader, quote_literal(&path))
    }
}

/// Every readable file under `root`, deepest-last and alphabetical.
fn walk(root: &Path) -> Vec<DataFile> {
    let mut files = Vec::new();
    let mut taken: HashSet<(String, String)> = HashSet::new();
    collect(root, root, 0, &mut files, &mut taken);
    files.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.name.cmp(&b.name)));
    files
}

fn collect(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<DataFile>,
    taken: &mut HashSet<(String, String)>,
) {
    if depth > MAX_DEPTH || files.len() >= MAX_FILES {
        return;
    }
    // An unreadable subdirectory should not fail the whole listing.
    let Ok(children) = std::fs::read_dir(directory) else {
        return;
    };

    let mut subdirectories = Vec::new();
    for child in children.flatten() {
        if files.len() >= MAX_FILES {
            return;
        }
        let path = child.path();
        let name = child.file_name().to_string_lossy().into_owned();
        let Ok(kind) = child.file_type() else {
            continue;
        };

        if kind.is_dir() {
            if !name.starts_with('.') && !crate::workspace::SKIP.contains(&name.as_str()) {
                subdirectories.push(path);
            }
        } else if kind.is_file() {
            if let Some(file) = describe_path(root, &path, taken) {
                files.push(file);
            }
        }
    }

    // Files before subdirectories, so the shallow ones win a name collision.
    for subdirectory in subdirectories {
        collect(root, &subdirectory, depth + 1, files, taken);
    }
}

/// Turn a path into a [`DataFile`], or `None` if alkyon cannot read that format.
fn describe_path(
    root: &Path,
    path: &Path,
    taken: &mut HashSet<(String, String)>,
) -> Option<DataFile> {
    let reader = reader_for(path)?;
    let filename = path.file_name()?.to_string_lossy().into_owned();
    let stem = path.file_stem()?.to_string_lossy().into_owned();

    let schema = match path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
    {
        Some(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().replace('\\', "/")
        }
        _ => ROOT_SCHEMA.to_owned(),
    };

    // `sales.csv` is `sales`; but if `sales.parquet` already claimed that name in
    // the same directory, the loser keeps its extension rather than vanishing.
    let name = if taken.insert((schema.clone(), stem.clone())) {
        stem
    } else {
        filename
    };
    taken.insert((schema.clone(), name.clone()));

    Some(DataFile {
        schema,
        name,
        path: path.to_path_buf(),
        reader,
    })
}

pub struct FilesConnector;

#[async_trait]
impl Connector for FilesConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let path = config
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "source `{}` is a folder or file source and needs a path",
                    config.id
                ))
            })?;

        // Resolved now rather than when the source was saved, so a project
        // registry stays portable and a disconnected drive shows a red dot
        // instead of having been baked in wrong.
        let root = crate::workspace::resolve_root(path)?;
        let single = root.is_file();
        if single && reader_for(&root).is_none() {
            return Err(Error::BadRequest(format!(
                "`{path}` is not a format alkyon reads directly ({}). A spreadsheet \
                 goes through `-- @excel` in a DuckDB buffer.",
                readable_extensions()
            )));
        }

        Ok(Box::new(FilesConnection { root, single }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
}

pub struct FilesConnection {
    /// The folder, or the single file the source points at.
    root: PathBuf,
    single: bool,
}

impl FilesConnection {
    /// A single file is granted as a file, not as its directory — pointing a
    /// source at one spreadsheet should not hand over everything beside it.
    fn sandbox(&self) -> Sandbox {
        if self.single {
            Sandbox::file(self.root.clone())
        } else {
            Sandbox::directory(self.root.clone())
        }
    }

    fn files(&self) -> Vec<DataFile> {
        if self.single {
            let mut taken = HashSet::new();
            let parent = self.root.parent().unwrap_or(&self.root);
            // The parent is only used to work out that there is no subdirectory,
            // so the single file lands in `main`.
            return describe_path(parent, &self.root, &mut taken)
                .into_iter()
                .collect();
        }
        walk(&self.root)
    }

    /// The files whose view name appears anywhere in `sql`.
    ///
    /// Creating a view makes DuckDB bind it, which reads the file's header — so
    /// defining every view on every query would mean reading two hundred headers
    /// to answer a query against one. A view can only be referenced if its name
    /// occurs in the text, so this drops work without ever dropping a table the
    /// query could have used.
    fn referenced(&self, sql: &str) -> Vec<DataFile> {
        let haystack = sql.to_lowercase();
        self.files()
            .into_iter()
            .filter(|file| haystack.contains(&file.name.to_lowercase()))
            .collect()
    }
}

/// Define `files` as views.
///
/// Creating a view makes DuckDB bind it, which reads the file's header — so this
/// is where a malformed CSV or a truncated parquet is found out. The error is
/// *returned* rather than swallowed: these are the files the query named, and
/// "the sniffer could not read line 4 of customers.csv" is a far better answer
/// than the `table customers does not exist` that skipping it would produce.
///
/// Enumeration paths — the tree, the snapshot — want the opposite and keep
/// going, so they do not call this.
fn define(connection: &duckdb::Connection, files: &[DataFile]) -> Result<()> {
    let mut schemas: Vec<&str> = Vec::new();
    for file in files {
        if file.schema != ROOT_SCHEMA && !schemas.contains(&file.schema.as_str()) {
            schemas.push(&file.schema);
            let sql = format!(
                "CREATE SCHEMA IF NOT EXISTS {};",
                quote_identifier(&file.schema)
            );
            connection.execute_batch(&sql).map_err(federated)?;
        }

        let sql = format!(
            "CREATE VIEW {}.{} AS SELECT * FROM {};",
            quote_identifier(&file.schema),
            quote_identifier(&file.name),
            file.scan()
        );
        connection
            .execute_batch(&sql)
            .map_err(|e| Error::Federated(format!("{}: {e}", file.path.display())))?;
    }
    Ok(())
}

/// The columns DuckDB infers for one file.
fn describe(connection: &duckdb::Connection, file: &DataFile) -> Result<Vec<ColumnInfo>> {
    let sql = format!("DESCRIBE SELECT * FROM {}", file.scan());
    let mut statement = connection.prepare(&sql).map_err(federated)?;
    let mut rows = statement.query([]).map_err(federated)?;

    let mut columns = Vec::new();
    let mut ordinal = 1;
    while let Some(row) = rows.next().map_err(federated)? {
        columns.push(ColumnInfo {
            name: row.get::<_, String>(0).map_err(federated)?,
            ordinal,
            data_type: row.get::<_, String>(1).map_err(federated)?.to_lowercase(),
            // A file has no constraints to report: everything may be missing and
            // nothing is a key.
            nullable: true,
            is_primary_key: false,
            default: None,
        });
        ordinal += 1;
    }
    Ok(columns)
}

fn federated(e: duckdb::Error) -> Error {
    Error::Federated(e.to_string())
}

#[async_trait]
impl Connection for FilesConnection {
    /// DuckDB's in-memory catalogue is called `memory`, and a folder source has
    /// exactly one. The level exists so the explorer tree keeps its shape.
    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(vec!["memory".to_owned()])
    }

    async fn list_tables(&self, _db: &str) -> Result<Vec<TableInfo>> {
        let files = self.files();
        Ok(files
            .into_iter()
            .map(|file| TableInfo {
                schema: file.schema,
                name: file.name,
                // A view is what it is in DuckDB, and it says "this is a file,
                // not something you can write to".
                kind: TableKind::View,
            })
            .collect())
    }

    async fn list_columns(&self, _db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let file = self
            .files()
            .into_iter()
            .find(|file| file.schema == schema && file.name == table)
            .ok_or_else(|| {
                Error::BadRequest(format!("no file `{schema}.{table}` in this source"))
            })?;
        let sandbox = self.sandbox();

        tokio::task::spawn_blocking(move || {
            let connection = open_duckdb(&sandbox)?;
            describe(&connection, &file)
        })
        .await
        .map_err(|e| Error::Federated(format!("reading the file failed: {e}")))?
    }

    async fn snapshot(&self, _db: &str) -> Result<Vec<TableSchema>> {
        let files = self.files();
        let sandbox = self.sandbox();

        tokio::task::spawn_blocking(move || {
            let connection = open_duckdb(&sandbox)?;
            let mut tables = Vec::new();
            for file in &files {
                // One unreadable file must not cost you the schema of the rest.
                match describe(&connection, file) {
                    Ok(columns) => tables.push(TableSchema {
                        schema: file.schema.clone(),
                        name: file.name.clone(),
                        kind: TableKind::View,
                        columns,
                    }),
                    Err(e) => {
                        tracing::warn!(file = %file.path.display(), error = %e, "skipped in snapshot")
                    }
                }
            }
            Ok(tables)
        })
        .await
        .map_err(|e| Error::Federated(format!("reading the folder failed: {e}")))?
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let files = self.referenced(sql);
            let sandbox = self.sandbox();
            let sql = sql.to_owned();
            let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

            // DuckDB is synchronous, so it gets its own thread rather than
            // stalling the runtime for the length of the query.
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                let connection = open_duckdb(&sandbox)?;
                define(&connection, &files)?;
                federation::run(&connection, &sql, &sink)
            });

            while let Some(batch) = source.recv().await {
                yield batch?;
            }
            worker
                .await
                .map_err(|e| Error::Federated(format!("the query panicked: {e}")))??;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sales")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("customers.csv"), "id,name\n1,ada\n").unwrap();
        std::fs::write(root.join("notes.md"), "ignored").unwrap();
        std::fs::write(root.join("sales/orders.csv"), "id\n1\n").unwrap();
        std::fs::write(root.join("node_modules/dep.csv"), "id\n9\n").unwrap();
        dir
    }

    #[test]
    fn subdirectories_become_schemas_and_noise_is_skipped() {
        let dir = fixture();
        let files = walk(dir.path());
        let found: Vec<(String, String)> = files
            .iter()
            .map(|f| (f.schema.clone(), f.name.clone()))
            .collect();

        assert_eq!(
            found,
            [
                ("main".to_owned(), "customers".to_owned()),
                ("sales".to_owned(), "orders".to_owned()),
            ],
            "node_modules should not be indexed, and .md is not a data file"
        );
    }

    #[test]
    fn two_files_with_one_stem_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sales.csv"), "id\n1\n").unwrap();
        std::fs::write(dir.path().join("sales.json"), "[{\"id\":1}]").unwrap();

        let names: HashSet<String> = walk(dir.path()).into_iter().map(|f| f.name).collect();
        assert_eq!(
            names.len(),
            2,
            "one file must not shadow the other: {names:?}"
        );
        assert!(names.contains("sales"));
    }

    #[test]
    fn only_named_tables_are_defined() {
        let dir = fixture();
        let connection = FilesConnection {
            root: dir.path().to_path_buf(),
            single: false,
        };
        let referenced = connection.referenced("select * from orders");
        assert_eq!(referenced.len(), 1);
        assert_eq!(referenced[0].name, "orders");
        assert!(connection.referenced("select 1").is_empty());
    }
}
