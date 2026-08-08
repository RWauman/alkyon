//! Folder and file sources, end to end.
//!
//! Unlike the database tests these need no server at all: a folder source is
//! read in-process by the DuckDB compiled into the binary, so everything here
//! runs on every machine, every time.

use alkyon::error::Result;
use alkyon::model::{RowBatch, SourceConfig, TableKind};
use alkyon::state::AppState;
use alkyon::vault::Vault;
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A folder of CSV — the ordinary case, and the one most of these tests want.
fn folder_source(id: &str, path: &Path) -> SourceConfig {
    source(id, "folder", path, json!({ "format": "csv" }))
}

fn file_source(id: &str, path: &Path) -> SourceConfig {
    source(id, "file", path, json!({}))
}

/// A folder or file source with format options, as `POST /sources` would send it.
fn source(id: &str, kind: &str, path: &Path, options: serde_json::Value) -> SourceConfig {
    serde_json::from_value(json!({
        "id": id,
        "kind": kind,
        "path": path.to_string_lossy(),
        "options": options,
        "auth": { "method": "none" },
    }))
    .expect("the POST /sources wire format for a folder source")
}

/// The folder a source points at. Named, rather than the tempdir itself, because
/// the root directory's own name is the root table's name — and a tempdir is
/// called something like `.tmpA1b2`, which is nobody's idea of a table.
fn root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("shop")
}

/// One file at the root and two in a subdirectory, so both the plain case and the
/// union have something to say.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    std::fs::create_dir_all(root.join("sales")).unwrap();
    std::fs::write(
        root.join("customers.csv"),
        "id,name,credit\n1,ada,1000.50\n2,grace,2000.25\n3,alan,3000.75\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sales/2023.csv"),
        "order_id,customer_id,total\n10,1,99.5\n11,1,10.0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sales/2024.csv"),
        "order_id,customer_id,total\n12,2,7.25\n",
    )
    .unwrap();
    std::fs::write(root.join("readme.md"), "not data").unwrap();
    dir
}

async fn query(state: &AppState, key: &str, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let connection = state.open(key, None).await?;
    collect(connection.execute(sql)).await
}

/// Run a `-- @duckdb` buffer and gather the whole result.
async fn federated(state: &AppState, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let program = alkyon::federation::program::parse(sql)?;
    collect(alkyon::federation::execute(
        state,
        program,
        alkyon::federation::Limits::default(),
    ))
    .await
}

async fn collect(
    mut batches: futures::stream::BoxStream<'_, Result<RowBatch>>,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    while let Some(batch) = batches.next().await {
        match batch? {
            RowBatch::Columns(meta) => columns = meta.iter().map(|c| c.name.clone()).collect(),
            RowBatch::Rows(batch) => rows.extend(batch),
            RowBatch::Affected(_) => {}
        }
    }
    Ok((columns, rows))
}

#[tokio::test]
async fn a_folder_is_queryable_as_a_source() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .expect("registering a folder needs no credential");

    // A file at the root is a table named after itself, with no reader function
    // in sight.
    let (columns, rows) = query(
        &state,
        "user:data",
        "select id, name from customers order by id",
    )
    .await
    .expect("querying a CSV as a table");

    assert_eq!(columns, ["id", "name"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1].as_str(), Some("ada"));
}

/// Several files in one directory are one table, and every row says which file it
/// came from — a folder of monthly exports being twelve tables was never useful.
#[tokio::test]
async fn a_directory_of_files_is_one_table_that_names_them() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:data",
        "select source_file, count(*) as n from sales group by source_file order by source_file",
    )
    .await
    .expect("two files unioned into one table");

    assert_eq!(columns, ["source_file", "n"]);
    assert_eq!(rows.len(), 2, "one row per file: {rows:?}");
    // The stem, not the path and not the extension.
    assert_eq!(rows[0][0].as_str(), Some("2023"));
    assert_eq!(rows[0][1].as_i64(), Some(2));
    assert_eq!(rows[1][0].as_str(), Some("2024"));
    assert_eq!(rows[1][1].as_i64(), Some(1));
}

/// The point of the whole thing: a root file and a directory, joined.
#[tokio::test]
async fn a_root_file_and_a_directory_join_like_any_other_tables() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:data",
        "select c.name, count(*) as orders, sum(o.total) as spent\n\
         from customers c join sales o on o.customer_id = c.id\n\
         group by c.name order by c.name",
    )
    .await
    .expect("joining a root file to a subdirectory's table");

    assert_eq!(columns, ["name", "orders", "spent"]);
    assert_eq!(rows.len(), 2, "ada and grace ordered, alan did not");
    assert_eq!(rows[0][0].as_str(), Some("ada"));
    assert_eq!(rows[0][1].as_i64(), Some(2));
}

/// Rigid on purpose: files that do not agree on their columns are an error, not a
/// best-effort reshaping that quietly loses a column.
///
/// DuckDB's own check, and it fires when the missing column is read — a `count(*)`
/// over the same table projects nothing and so notices nothing. Alkyon does not
/// try to improve on that: pre-reading every header to fail earlier would cost the
/// header reads this connector exists to avoid.
#[tokio::test]
async fn files_that_disagree_on_their_columns_are_an_error() {
    let dir = fixture();
    std::fs::write(
        root(&dir).join("sales/2025.csv"),
        "order_id,customer_id\n13,3\n",
    )
    .unwrap();

    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let Err(error) = query(&state, "user:data", "select order_id, total from sales").await else {
        panic!("three columns and two columns cannot be one table");
    };
    let error = error.to_string();
    assert!(error.contains("2025.csv"), "say which file: {error}");
    assert!(error.contains("total"), "say which column: {error}");
}

/// Files left out when the source was registered: a file is excluded, and nothing
/// else about the folder changes.
#[tokio::test]
async fn excluded_files_are_left_out() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(source(
            "without-2023",
            "folder",
            &root(&dir),
            json!({ "format": "csv", "exclude": ["sales/2023.csv"] }),
        ))
        .await
        .unwrap();

    let connection = state.open("user:without-2023", None).await.unwrap();
    let mut tables = connection.list_tables("shop").await.unwrap();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["customers", "sales"], "only a file was excluded");

    let (_, rows) = query(
        &state,
        "user:without-2023",
        "select count(*), min(source_file) from sales",
    )
    .await
    .unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(1), "2023.csv is not in it");
    assert_eq!(rows[0][1].as_str(), Some("2024"));

    // A directory left with nothing is not a table at all.
    state
        .register(source(
            "no-sales",
            "folder",
            &root(&dir),
            json!({
                "format": "csv",
                "exclude": ["sales/2023.csv", "sales/2024.csv"],
            }),
        ))
        .await
        .unwrap();
    let connection = state.open("user:no-sales", None).await.unwrap();
    let tables = connection.list_tables("shop").await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["customers"]);
}

/// Types come from DuckDB's own inference, not from everything being text.
#[tokio::test]
async fn columns_are_typed_not_all_text() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let (_, rows) = query(
        &state,
        "user:data",
        "select typeof(id), typeof(name), typeof(credit) from customers limit 1",
    )
    .await
    .unwrap();

    assert_eq!(rows[0][0].as_str(), Some("BIGINT"));
    assert_eq!(rows[0][1].as_str(), Some("VARCHAR"));
    assert_eq!(rows[0][2].as_str(), Some("DOUBLE"));
}

#[tokio::test]
async fn the_explorer_sees_root_files_and_directories_as_tables() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();
    let connection = state.open("user:data", None).await.unwrap();

    // The catalogue takes the folder's name — `memory` says nothing about what you
    // are looking at, and the tree reads `shop → public → customers`.
    assert_eq!(connection.list_databases().await.unwrap(), ["shop"]);

    let mut tables = connection.list_tables("shop").await.unwrap();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let seen: Vec<(String, String)> = tables
        .iter()
        .map(|t| (t.schema.clone(), t.name.clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("public".to_owned(), "customers".to_owned()),
            ("public".to_owned(), "sales".to_owned()),
        ],
        "a root file and a subdirectory, both in `public`; readme.md is not data"
    );
    assert!(tables.iter().all(|t| t.kind == TableKind::View));

    // Columns and their inferred types, which is what autocompletion and the
    // Ctrl+K search index are built from. A root file is one named file, so it has
    // no file column.
    let columns = connection
        .list_columns("shop", "public", "customers")
        .await
        .unwrap();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "credit"]);
    assert_eq!(columns[2].data_type, "double");

    // The subdirectory's does, and it is there to be selected and grouped by.
    let columns = connection
        .list_columns("shop", "public", "sales")
        .await
        .unwrap();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["order_id", "customer_id", "total", "source_file"]);

    let snapshot = connection.snapshot("shop").await.unwrap();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.iter().all(|table| !table.columns.is_empty()));
}

/// A subdirectory named `2022` becomes a table named `2022`, and SQL reads that
/// as a number unless it is quoted.
///
/// Nothing alkyon can fix — renaming it would make the tree lie about the
/// folder. What alkyon owes you is that everything *it* writes is quoted, and
/// that the error says so.
#[tokio::test]
async fn a_table_that_is_not_a_bare_identifier_must_be_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("taxi");
    std::fs::create_dir_all(root.join("2022")).unwrap();
    std::fs::write(root.join("2022/trips.csv"), "id\n1\n2\n").unwrap();
    std::fs::write(root.join("zones.csv"), "id\n9\n").unwrap();

    let state = AppState::new();
    state.register(folder_source("taxi", &root)).await.unwrap();

    // A name that *is* a bare identifier needs nothing.
    let (_, rows) = query(&state, "user:taxi", "select * from public.zones")
        .await
        .expect("public.zones");
    assert_eq!(rows.len(), 1);

    // Quoted, the digit-named one works too.
    let (_, rows) = query(&state, "user:taxi", "select id from \"2022\"")
        .await
        .expect("\"2022\"");
    assert_eq!(rows.len(), 2);

    // Unquoted it cannot: `2022` is a number. The error has to say which tables
    // need the quotes, because DuckDB's own message is only "syntax error".
    let Err(error) = query(&state, "user:taxi", "select id from 2022 order by id").await else {
        panic!("`from 2022` is not valid SQL");
    };
    let error = error.to_string();
    assert!(error.contains("\"2022\""), "name the table: {error}");
    assert!(error.contains("quote"), "say what to do about it: {error}");
}

/// A folder source is confined to its folder, the same way a federated session
/// is confined to the open one.
#[tokio::test]
async fn a_folder_source_cannot_read_outside_itself() {
    let dir = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.csv"), "a\n1\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let escape = outside
        .path()
        .join("secret.csv")
        .to_string_lossy()
        .replace('\\', "/");
    for attempt in [
        format!("select * from read_csv('{escape}')"),
        format!("copy (select 1 as x) to '{escape}'"),
        "select * from read_csv('C:/Windows/win.ini')".to_owned(),
        "install httpfs".to_owned(),
        "set enable_external_access = true".to_owned(),
    ] {
        assert!(
            query(&state, "user:data", &attempt).await.is_err(),
            "`{attempt}` should have been refused"
        );
    }

    // But writing *inside* the folder is allowed — that is the export path.
    let target = root(&dir)
        .join("out.parquet")
        .to_string_lossy()
        .replace('\\', "/");
    query(
        &state,
        "user:data",
        &format!("copy (select * from customers) to '{target}' (format parquet)"),
    )
    .await
    .expect("exporting into the folder");
    assert!(root(&dir).join("out.parquet").is_file());
}

/// Pointing a source at one file must not hand over its neighbours.
#[tokio::test]
async fn a_single_file_source_exposes_only_that_file() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(file_source("one", &root(&dir).join("customers.csv")))
        .await
        .unwrap();

    let connection = state.open("user:one", None).await.unwrap();
    // A file source shows the directory it sits in, since its own name is already
    // the table's: `shop → public → customers`.
    assert_eq!(connection.list_databases().await.unwrap(), ["shop"]);

    let tables = connection.list_tables("shop").await.unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].schema, "public");
    assert_eq!(tables[0].name, "customers");

    // One file has nothing to tell apart, so it gets no file column.
    let columns = connection
        .list_columns("shop", "public", "customers")
        .await
        .unwrap();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "credit"]);

    let (_, rows) = query(&state, "user:one", "select count(*) from customers")
        .await
        .unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(3));

    // The sibling sits in the same directory and is still out of reach: the
    // session is granted the file, not the folder around it.
    let sibling = root(&dir)
        .join("sales/2023.csv")
        .to_string_lossy()
        .replace('\\', "/");
    assert!(
        query(
            &state,
            "user:one",
            &format!("select * from read_csv('{sibling}')")
        )
        .await
        .is_err(),
        "a file source must not reach the rest of its directory"
    );
}

/// A file DuckDB refuses must say *why*, and name itself.
///
/// The tempting alternative — skip it and carry on — turns a readable
/// "line 4 has one column too many" into `table budget does not exist`, which
/// sends you looking for a typo that is not there.
#[tokio::test]
async fn an_unreadable_file_reports_the_real_reason() {
    let dir = fixture();
    // Mixed line endings: DuckDB's sniffer refuses these, and a spreadsheet
    // exported on the wrong platform is exactly how you get one.
    std::fs::create_dir_all(root(&dir).join("budget")).unwrap();
    std::fs::write(root(&dir).join("budget/budget.csv"), "a,b\n1,2\n3,4\r\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let Err(error) = query(&state, "user:data", "select * from budget").await else {
        panic!("a malformed CSV should not be silently empty");
    };
    let error = error.to_string();
    assert!(error.contains("budget.csv"), "say which file: {error}");
    assert!(
        !error.contains("does not exist"),
        "the file exists; the message must not suggest a typo: {error}"
    );

    // And it costs you that directory only — the rest of the folder still works.
    let (_, rows) = query(&state, "user:data", "select count(*) from customers")
        .await
        .expect("one bad file must not take the source down");
    assert_eq!(rows[0][0].as_i64(), Some(3));
}

#[tokio::test]
async fn a_file_alkyon_cannot_read_says_so() {
    let dir = fixture();
    let state = AppState::new();
    let error = state
        .register(file_source("notes", &root(&dir).join("readme.md")))
        .await
        .err();

    // `register` itself does not connect — the API route does — so reach for the
    // connector the way `POST /sources` does.
    assert!(error.is_none());
    let Err(error) = state.open("user:notes", None).await else {
        panic!("markdown is not a data file");
    };
    let error = error.to_string();
    assert!(error.contains("not a format alkyon reads"), "{error}");
    assert!(error.contains("parquet"), "say what it does read: {error}");
}

/// The kind is a promise about the path, not a guess from it.
#[tokio::test]
async fn a_folder_and_a_file_are_not_interchangeable() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(file_source("wrong-file", &root(&dir)))
        .await
        .unwrap();
    state
        .register(folder_source(
            "wrong-folder",
            &root(&dir).join("customers.csv"),
        ))
        .await
        .unwrap();

    let Err(error) = state.open("user:wrong-file", None).await else {
        panic!("a folder registered as a file should be refused");
    };
    assert!(error.to_string().contains("is a folder"), "{error}");

    let Err(error) = state.open("user:wrong-folder", None).await else {
        panic!("a file registered as a folder should be refused");
    };
    assert!(error.to_string().contains("is a file"), "{error}");
}

/// Which type a folder holds is not guessed: files of two formats cannot be one
/// table, so the source has to say.
#[tokio::test]
async fn a_folder_must_declare_its_file_type() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(source("undeclared", "folder", &root(&dir), json!({})))
        .await
        .unwrap();

    let Err(error) = state.open("user:undeclared", None).await else {
        panic!("a folder source without a file type should be refused");
    };
    let error = error.to_string();
    assert!(error.contains("which file type"), "{error}");
    assert!(error.contains("parquet"), "list them: {error}");
}

/// Declaring a format narrows the folder to that format's own extensions.
#[tokio::test]
async fn the_declared_format_excludes_the_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("mixed");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.csv"), "x\n1\n").unwrap();
    std::fs::write(root.join("b.json"), "[{\"y\": 2}]").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("as-csv", &root))
        .await
        .unwrap();
    state
        .register(source(
            "as-json",
            "folder",
            &root,
            json!({ "format": "json" }),
        ))
        .await
        .unwrap();

    // Each source sees one of the two files, and neither sees a table for the
    // other's.
    let (columns, _) = query(&state, "user:as-csv", "select * from a")
        .await
        .expect("only the CSV");
    assert_eq!(columns, ["x"]);
    assert!(
        query(&state, "user:as-csv", "select * from b")
            .await
            .is_err(),
        "the JSON is not data to a CSV source"
    );

    let (columns, _) = query(&state, "user:as-json", "select * from b")
        .await
        .expect("only the JSON");
    assert_eq!(columns, ["y"]);
}

/// JSON, and JSON lines, which are the same reader told two different things.
#[tokio::test]
async fn json_and_json_lines_are_both_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(root.join("documents")).unwrap();
    std::fs::create_dir_all(root.join("lines")).unwrap();
    std::fs::write(
        root.join("documents/docs.json"),
        "[{\"id\": 1, \"name\": \"ada\"}, {\"id\": 2, \"name\": \"grace\"}]",
    )
    .unwrap();
    std::fs::write(
        root.join("lines/log.jsonl"),
        "{\"id\": 1, \"name\": \"ada\"}\n{\"id\": 2, \"name\": \"grace\"}\n{\"id\": 3, \"name\": \"alan\"}\n",
    )
    .unwrap();

    let state = AppState::new();
    state
        .register(source("docs", "folder", &root, json!({ "format": "json" })))
        .await
        .unwrap();
    state
        .register(source(
            "log",
            "folder",
            &root,
            json!({ "format": "json_lines" }),
        ))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:docs",
        "select id, name from documents order by id",
    )
    .await
    .expect("a JSON array of documents");
    assert_eq!(columns, ["id", "name"]);
    assert_eq!(rows.len(), 2);

    let (_, rows) = query(&state, "user:log", "select count(*) from lines")
        .await
        .expect("one document per line");
    assert_eq!(rows[0][0].as_i64(), Some(3));
}

/// The CSV the sniffer gets wrong: semicolons, comma decimals, and a preamble.
///
/// This is what the options exist for, so it is worth a test that would fail
/// without every one of them.
#[tokio::test]
async fn csv_options_reach_duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ventes.csv");
    std::fs::write(
        &path,
        "Export du 1er fevrier\nGenere automatiquement\nid;nom;credit\n1;ada;1000,50\n2;grace;2000,25\n",
    )
    .unwrap();

    let state = AppState::new();
    state
        .register(source("plain", "file", &path, json!({})))
        .await
        .unwrap();

    // DuckDB's sniffer is better than this test first assumed: it works out the
    // `;` and the two lines of preamble on its own. What it cannot guess is the
    // *decimal comma* — `1000,50` is a perfectly good string — so untuned, a money
    // column arrives as text and every sum over it fails.
    let (_, rows) = query(
        &state,
        "user:plain",
        "select typeof(credit) as kind from ventes limit 1",
    )
    .await
    .expect("the sniffer handles the delimiter and the preamble");
    assert_eq!(
        rows[0][0].as_str(),
        Some("VARCHAR"),
        "a decimal comma cannot be sniffed, so this is text until it is declared"
    );

    state
        .register(source(
            "tuned",
            "file",
            &path,
            json!({
                "format": "csv",
                "csv": { "delimiter": ";", "decimal": ",", "skip": 2 },
            }),
        ))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:tuned",
        "select id, nom, credit, typeof(credit) as kind from ventes order by id",
    )
    .await
    .expect("the options should make it readable");

    assert_eq!(columns, ["id", "nom", "credit", "kind"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1].as_str(), Some("ada"));
    // The decimal separator is the point: as text this column would be VARCHAR.
    assert!(
        rows[0][3].as_str().is_some_and(|kind| kind != "VARCHAR"),
        "credit should have become a number, got {:?}",
        rows[0][3]
    );
}

/// A declared format beats the extension, which is how `export.dat` is read.
#[tokio::test]
async fn a_declared_format_overrides_the_extension_for_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.dat");
    std::fs::write(&path, "id,name\n1,ada\n").unwrap();

    let state = AppState::new();
    state
        .register(source("unknown", "file", &path, json!({})))
        .await
        .unwrap();
    let Err(error) = state.open("user:unknown", None).await else {
        panic!("`.dat` means nothing on its own");
    };
    assert!(
        error.to_string().contains("not a format alkyon reads"),
        "{error}"
    );

    state
        .register(source(
            "declared",
            "file",
            &path,
            json!({ "format": "csv" }),
        ))
        .await
        .unwrap();
    let (_, rows) = query(&state, "user:declared", "select count(*) from export")
        .await
        .expect("declared as CSV");
    assert_eq!(rows[0][0].as_i64(), Some(1));
}

/// The committed workbooks, copied into a folder each — a directory is a table, so
/// two unrelated workbooks do not belong in one.
fn workbooks() -> tempfile::TempDir {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sheets");
    std::fs::create_dir_all(root.join("books")).unwrap();
    std::fs::create_dir_all(root.join("plans")).unwrap();
    std::fs::copy(fixtures.join("book.xlsx"), root.join("books/book.xlsx")).unwrap();
    std::fs::copy(fixtures.join("budget.xlsx"), root.join("plans/budget.xlsx")).unwrap();
    dir
}

/// Excel, against real workbooks committed under `tests/fixtures/`.
///
/// A committed binary rather than one built on the fly: writing a valid `.xlsx`
/// would mean a writer crate alkyon does not otherwise need, and the whole point is
/// to read a file Excel itself produced.
#[tokio::test]
async fn a_workbook_becomes_one_table_per_sheet() {
    let dir = workbooks();
    let state = AppState::new();
    state
        .register(source(
            "books",
            "folder",
            &dir.path().join("sheets"),
            json!({ "format": "excel" }),
        ))
        .await
        .unwrap();

    let connection = state.open("user:books", None).await.unwrap();
    let mut names: Vec<String> = connection
        .list_tables("sheets")
        .await
        .unwrap()
        .into_iter()
        .map(|table| table.name)
        .collect();
    names.sort();
    // Two sheets means two tables, named for the directory and the sheet; one
    // sheet keeps the directory's own name. The tree has to agree with what a
    // query can reach.
    assert_eq!(names, ["books_Customers", "books_Orders", "plans"]);

    let (columns, rows) = query(
        &state,
        "user:books",
        "select name, credit from books_Customers order by id",
    )
    .await
    .expect("a sheet as a table");
    assert_eq!(columns, ["name", "credit"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0].as_str(), Some("ada"));
    // An empty cell is NULL, not the text "None".
    assert_eq!(rows[2][1], Value::Null);

    // Calamine reports the types it inferred, so a number stays summable — which
    // is the whole difference between a spreadsheet read and a screenshot of one.
    let (_, rows) = query(
        &state,
        "user:books",
        "select sum(credit) as total from books_Customers",
    )
    .await
    .expect("summing a spreadsheet column");
    assert_eq!(rows[0][0].as_f64(), Some(3000.75));

    // Two sheets of one workbook join like any other pair of tables.
    let (_, rows) = query(
        &state,
        "user:books",
        "select count(*) from books_Customers c join books_Orders o on o.customer_id = c.id",
    )
    .await
    .expect("joining two sheets");
    assert_eq!(rows[0][0].as_i64(), Some(3));
}

/// Spreadsheets union like every other format, calamine rather than DuckDB doing
/// the reading — so the check that they agree, and the file column, are alkyon's
/// own work here.
#[tokio::test]
async fn workbooks_in_one_subdirectory_are_unioned() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sheets");
    // In a subdirectory, since that is what a union is: files at the root are
    // tables of their own.
    let monthly = root.join("monthly");
    std::fs::create_dir_all(&monthly).unwrap();
    std::fs::copy(fixtures.join("book.xlsx"), monthly.join("january.xlsx")).unwrap();
    std::fs::copy(fixtures.join("book.xlsx"), monthly.join("february.xlsx")).unwrap();

    let state = AppState::new();
    state
        .register(source(
            "monthly",
            "folder",
            &root,
            json!({ "format": "excel", "excel": { "sheet": "Customers" } }),
        ))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:monthly",
        "select source_file, count(*) as n from monthly group by source_file order by source_file",
    )
    .await
    .expect("two workbooks as one table");

    assert_eq!(columns, ["source_file", "n"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].as_str(), Some("february"));
    assert_eq!(rows[0][1].as_i64(), Some(3));

    // A workbook that does not match is named, rather than failing as a cast three
    // columns later.
    std::fs::copy(fixtures.join("budget.xlsx"), monthly.join("march.xlsx")).unwrap();
    let Err(error) = query(&state, "user:monthly", "select count(*) from monthly").await else {
        panic!("a workbook with other columns cannot join the union");
    };
    let error = error.to_string();
    assert!(error.contains("march.xlsx"), "say which file: {error}");
}

/// One sheet of one workbook, named in the options.
#[tokio::test]
async fn a_named_sheet_is_the_only_table() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/book.xlsx");
    let state = AppState::new();
    state
        .register(source(
            "orders-only",
            "file",
            &path,
            json!({ "format": "excel", "excel": { "sheet": "Orders" } }),
        ))
        .await
        .unwrap();

    let connection = state.open("user:orders-only", None).await.unwrap();
    let tables = connection.list_tables("fixtures").await.unwrap();
    assert_eq!(tables.len(), 1);
    // Named explicitly, so the table is the file's own name and not `book_Orders`.
    assert_eq!(tables[0].name, "book");

    let (_, rows) = query(&state, "user:orders-only", "select count(*) from book")
        .await
        .unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(3));
}

/// A project source's relative path is relative to the project.
///
/// That is what lets a committed `.alkyon/sources.json` say `./sample-data/csv`
/// and mean the same folder on every machine.
#[tokio::test]
async fn a_project_source_resolves_its_path_against_the_open_folder() {
    let dir = fixture();
    let config = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), config.path(), true).unwrap();
    state
        .open_workspace(root(&dir).to_str().unwrap())
        .await
        .unwrap();

    let mut relative = folder_source("here", Path::new("./sales"));
    relative.scope = alkyon::model::Scope::Project;
    state.register(relative).await.unwrap();

    // `sales/` is this source's own root, so its files are its tables.
    let (_, rows) = query(&state, "project:here", "select count(*) from \"2023\"")
        .await
        .expect("`./sales` should mean the open folder's `sales`");
    assert_eq!(rows[0][0].as_i64(), Some(2));

    // The same path in the *user* registry has nothing to be relative to, and
    // resolving it against the server's working directory would work exactly once.
    let mut user = folder_source("nowhere", Path::new("./sales"));
    user.scope = alkyon::model::Scope::User;
    state.register(user).await.unwrap();
    let Err(error) = state.open("user:nowhere", None).await else {
        panic!("a relative path in a user source should be refused");
    };
    assert!(error.to_string().contains("relative path"), "{error}");
}

/// A folder source is a source, so `@import` reaches it like any other — which
/// is what makes "join a database table to a parquet file" work without a single
/// line of federation code that knows about folders.
#[tokio::test]
async fn a_folder_source_can_be_imported_into_a_federated_query() {
    let dir = fixture();
    let other = tempfile::tempdir().unwrap();
    let targets = other.path().join("budget");
    std::fs::create_dir_all(&targets).unwrap();
    std::fs::write(
        targets.join("targets.csv"),
        "name,target\nada,500\ngrace,900\n",
    )
    .unwrap();

    let state = AppState::new();
    state
        .register(folder_source("sales", &root(&dir)))
        .await
        .unwrap();
    state
        .register(folder_source("budget", &targets))
        .await
        .unwrap();

    let (columns, rows) = federated(
        &state,
        "-- @duckdb\n\
         -- @import c = sales : select id, name, credit from customers\n\
         -- @import t = budget : select name, target from targets\n\
         select c.name, c.credit, t.target from c join t on t.name = c.name order by c.name;",
    )
    .await
    .expect("federating two folder sources");

    assert_eq!(columns, ["name", "credit", "target"]);
    assert_eq!(rows.len(), 2, "ada and grace are in both folders");
    assert_eq!(rows[0][0].as_str(), Some("ada"));
    assert_eq!(rows[0][2].as_i64(), Some(500));
}

/// `@import x = source/*.parquet` — no SQL, so DuckDB reads the files itself.
///
/// The point is what does *not* happen: a materialised import turns every cell
/// into a String in this process. A scan is a view, so a glob over a folder costs
/// the file headers and nothing more until the query asks for rows.
#[tokio::test]
async fn a_glob_import_unions_files_without_copying_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("trips");
    std::fs::create_dir_all(root.join("2022")).unwrap();
    std::fs::write(root.join("2022/jan.csv"), "id,total\n1,10\n2,20\n").unwrap();
    std::fs::write(root.join("2022/feb.csv"), "id,total\n3,30\n").unwrap();
    std::fs::write(root.join("other.csv"), "id,total\n9,90\n").unwrap();

    let state = AppState::new();
    state.register(folder_source("trips", &root)).await.unwrap();

    let (columns, rows) = federated(
        &state,
        "-- @duckdb\n\
         -- @import t = trips/2022/*.csv\n\
         select count(*) as n, sum(total) as total from t;",
    )
    .await
    .expect("a glob import");

    assert_eq!(columns, ["n", "total"]);
    // Both January files, and *not* the one outside the pattern.
    assert_eq!(rows[0][0].as_i64(), Some(3));
    assert_eq!(rows[0][1].as_i64(), Some(60));
}

/// A glob import still cannot reach past the source it names.
#[tokio::test]
async fn a_glob_import_stays_inside_its_source() {
    let dir = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.csv"), "a\n1\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    // Refused by the parser, before any path is built.
    for pattern in ["../*.csv", "a/../../secret.csv"] {
        let sql = format!("-- @duckdb\n-- @import x = data/{pattern}\nselect * from x;");
        assert!(
            federated(&state, &sql).await.is_err(),
            "`{pattern}` should have been refused"
        );
    }

    // And a database source has SQL to run, so it cannot be imported by path.
    state
        .register(
            serde_json::from_value(json!({
                "id": "pg", "kind": "postgres", "host": "db.example.com",
                "auth": { "method": "password", "username": "u", "password": "p" },
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let Err(error) = federated(
        &state,
        "-- @duckdb\n-- @import x = pg/whatever.parquet\nselect * from x;",
    )
    .await
    else {
        panic!("a postgres source has no files to glob");
    };
    assert!(error.to_string().contains("needs SQL to run"), "{error}");
}

#[tokio::test]
async fn a_folder_source_is_persisted_without_a_credential() {
    let dir = fixture();
    let config = tempfile::tempdir().unwrap();

    let first = AppState::load(Vault::memory(), config.path(), true).unwrap();
    first
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let raw = std::fs::read_to_string(config.path().join("sources.json")).unwrap();
    assert!(raw.contains("\"kind\": \"folder\""), "{raw}");
    assert!(raw.contains("\"path\""), "{raw}");
    assert!(!raw.contains("\"host\""), "a folder has no host: {raw}");

    // A fresh vault loses nothing, because there was never a secret to lose —
    // this is the case that fails for a database source.
    let second = AppState::load(Vault::memory(), config.path(), true).unwrap();
    let summaries = second.summaries().await;
    assert_eq!(summaries[0].auth_method, "none");
    assert_eq!(summaries[0].dialect, alkyon::model::Dialect::DuckDb);
    assert_eq!(summaries[0].editor_mime, "text/x-sql");
    second
        .open("user:data", None)
        .await
        .expect("a folder source works after a restart with an empty vault");
}

/// A Delta table, written by hand: two parquet files, one of which the log says
/// was replaced. `delta_scan` must read the live one and ignore the other.
///
/// Written rather than generated because that is the whole point — a directory of
/// parquet unioned blindly gives four rows, and the log is what makes it two.
fn delta_fixture(dir: &Path) -> std::io::Result<()> {
    let table = dir.join("sales");
    std::fs::create_dir_all(table.join("_delta_log"))?;

    // Two parquet, written with the DuckDB inside alkyon so the schema is real.
    let connection = duckdb::Connection::open_in_memory().unwrap();
    for (file, values) in [("old.parquet", "(1, 'stale'), (2, 'stale')"), ("new.parquet", "(1, 'live'), (2, 'live')")] {
        let to = table.join(file).to_string_lossy().replace(char::from(92), "/");
        connection
            .execute_batch(&format!(
                "COPY (SELECT * FROM (VALUES {values}) AS t(id, note)) TO '{to}' (FORMAT parquet)"
            ))
            .unwrap();
    }

    let size = |file: &str| std::fs::metadata(table.join(file)).unwrap().len();
    let schema = r#"{\"type\":\"struct\",\"fields\":[{\"name\":\"id\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"note\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}}]}"#;
    // One JSON document per line, which is what a Delta commit is.
    let protocol = r#"{"protocol":{"minReaderVersion":1,"minWriterVersion":2}}"#;
    let metadata = format!(
        r#"{{"metaData":{{"id":"alkyon-test","format":{{"provider":"parquet","options":{{}}}},"schemaString":"{schema}","partitionColumns":[],"configuration":{{}},"createdTime":0}}}}"#
    );
    let added = format!(
        r#"{{"add":{{"path":"new.parquet","size":{},"partitionValues":{{}},"modificationTime":1,"dataChange":true}}}}"#,
        size("new.parquet")
    );
    // Present on disk, absent from the table: this is what the log is for.
    let removed = format!(
        r#"{{"remove":{{"path":"old.parquet","size":{},"partitionValues":{{}},"deletionTimestamp":2,"dataChange":true}}}}"#,
        size("old.parquet")
    );
    let log = format!("{protocol}\n{metadata}\n{added}\n{removed}\n");
    std::fs::write(table.join("_delta_log/00000000000000000000.json"), log)
}

#[tokio::test]
async fn a_delta_table_is_read_through_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lake");
    std::fs::create_dir_all(&root).unwrap();
    delta_fixture(&root).expect("a hand-written Delta table");

    let state = AppState::new();
    state
        .register(source("lake", "folder", &root, json!({ "format": "delta" })))
        .await
        .expect("registering a Delta folder");

    // The directory holding `_delta_log` is the table, named after itself.
    let connection = state.open("lake", None).await.unwrap();
    let tables = connection.list_tables("lake").await.unwrap();
    assert_eq!(
        tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["sales"],
        "one table, not one per parquet"
    );

    let (columns, rows) = query(&state, "lake", "select note, count(*) as n from sales group by note")
        .await
        .expect("querying a Delta table");
    assert_eq!(columns, ["note", "n"]);
    assert_eq!(
        rows,
        [[json!("live"), json!(2)]],
        "the removed file must not be read: a blind union would give 'stale' too"
    );
}

/// Every DuckDB type a source can return, as the grid receives it.
///
/// This is a regression test for output that was Rust's `Debug`: a date arrived as
/// `Date32(20455)`, a total as `Decimal(Decimal { width: 38, scale: 2, value: … })`
/// and a struct as `Struct(OrderedMap([…]))`. It went unnoticed because the sample
/// data carries its dates as text.
#[tokio::test]
async fn every_duckdb_type_arrives_as_something_readable() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "data",
        "select cast(1234.56 as decimal(18,2)) as money,
                date '2026-01-02' as day,
                timestamp '2026-01-02 03:04:05.5' as moment,
                time '03:04:05' as clock,
                [10, 20] as list,
                {'city': 'Brussels', 'zip': 1000} as nested,
                cast(9223372036854775808 as hugeint) as beyond_i64",
    )
    .await
    .expect("a query returning one of everything");

    assert_eq!(
        columns,
        ["money", "day", "moment", "clock", "list", "nested", "beyond_i64"]
    );
    let row = &rows[0];
    // Exact digits, as a string — the same way `numeric` travels from PostgreSQL.
    assert_eq!(row[0], json!("1234.56"));
    assert_eq!(row[1], json!("2026-01-02"));
    // The fraction keeps the precision the value has, and is absent when there is
    // none — no trailing `.000000` on a whole second.
    assert_eq!(row[2], json!("2026-01-02 03:04:05.500"));
    assert_eq!(row[3], json!("03:04:05"));
    // Structure, not a debug dump — and still `unnest`-able in SQL.
    assert_eq!(row[4], json!([10, 20]));
    assert_eq!(row[5], json!({ "city": "Brussels", "zip": 1000 }));
    // Past what a JSON number holds, so the digits go as text rather than drift.
    assert_eq!(row[6], json!("9223372036854775808"));

    let (_, rows) = query(&state, "data", "select timestamp '2026-01-02 03:04:05' as t")
        .await
        .unwrap();
    assert_eq!(rows[0][0], json!("2026-01-02 03:04:05"));
}

/// `INTERVAL` is the one type the DuckDB crate cannot hand back: mapping its Arrow
/// type panics inside the driver with `not implemented: Interval(MonthDayNano)`.
///
/// Pinned rather than fixed, because the fix is not ours to make. What matters is
/// that it stays *contained* — an error for that query, not a process that dies
/// with the rest of the session in it. Cast it and it reads fine.
#[tokio::test]
async fn an_interval_column_fails_without_taking_the_session_down() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", &root(&dir)))
        .await
        .unwrap();

    let error = query(&state, "data", "select interval '1 month' as gap")
        .await
        .expect_err("the driver cannot represent an interval");
    assert!(error.to_string().contains("Interval"), "{error}");

    // The session is still there, and the way round it works.
    let (_, rows) = query(
        &state,
        "data",
        "select cast(interval '1 month 3 days' as varchar) as gap",
    )
    .await
    .expect("cast to text and it is readable");
    assert_eq!(rows[0][0], json!("1 month 3 days"));
}
