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
use std::path::Path;

fn folder_source(id: &str, path: &Path) -> SourceConfig {
    serde_json::from_value(json!({
        "id": id,
        "kind": "files",
        "path": path.to_string_lossy(),
        "auth": { "method": "none" },
    }))
    .expect("the POST /sources wire format for a folder source")
}

/// A folder with a couple of files in it, one of them in a subdirectory.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("sales")).unwrap();
    std::fs::write(
        root.join("customers.csv"),
        "id,name,credit\n1,ada,1000.50\n2,grace,2000.25\n3,alan,3000.75\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sales/orders.csv"),
        "order_id,customer_id,total\n10,1,99.5\n11,1,10.0\n12,2,7.25\n",
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
        .register(folder_source("data", dir.path()))
        .await
        .expect("registering a folder needs no credential");

    // The file is a table, named after itself, with no reader function in sight.
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

/// The point of the whole thing: two files in one folder, joined.
#[tokio::test]
async fn a_subdirectory_is_a_schema_and_files_join() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", dir.path()))
        .await
        .unwrap();

    let (columns, rows) = query(
        &state,
        "user:data",
        "select c.name, count(*) as orders, sum(o.total) as spent\n\
         from customers c join sales.orders o on o.customer_id = c.id\n\
         group by c.name order by c.name",
    )
    .await
    .expect("joining a root file to one in a subdirectory");

    assert_eq!(columns, ["name", "orders", "spent"]);
    assert_eq!(rows.len(), 2, "ada and grace ordered, alan did not");
    assert_eq!(rows[0][0].as_str(), Some("ada"));
    assert_eq!(rows[0][1].as_i64(), Some(2));
}

/// Types come from DuckDB's own inference, not from everything being text.
#[tokio::test]
async fn columns_are_typed_not_all_text() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", dir.path()))
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
async fn the_explorer_sees_the_files_as_tables() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("data", dir.path()))
        .await
        .unwrap();
    let connection = state.open("user:data", None).await.unwrap();

    assert_eq!(connection.list_databases().await.unwrap(), ["memory"]);

    let mut tables = connection.list_tables("memory").await.unwrap();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let seen: Vec<(String, String)> = tables
        .iter()
        .map(|t| (t.schema.clone(), t.name.clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("main".to_owned(), "customers".to_owned()),
            ("sales".to_owned(), "orders".to_owned()),
        ],
        "readme.md is not a data file"
    );
    assert!(tables.iter().all(|t| t.kind == TableKind::View));

    // Columns and their inferred types, which is what autocompletion and the
    // Ctrl+K search index are built from.
    let columns = connection
        .list_columns("memory", "main", "customers")
        .await
        .unwrap();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "credit"]);
    assert_eq!(columns[2].data_type, "double");

    let snapshot = connection.snapshot("memory").await.unwrap();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.iter().all(|table| !table.columns.is_empty()));
}

/// A subdirectory named `2022` becomes a schema named `2022`, and SQL reads that
/// as a number unless it is quoted.
///
/// Nothing alkyon can fix — renaming it would make the tree lie about the
/// folder. What alkyon owes you is that everything *it* writes is quoted, and
/// that the error says so.
#[tokio::test]
async fn a_schema_that_is_not_a_bare_identifier_must_be_quoted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("2022")).unwrap();
    std::fs::write(dir.path().join("2022/trips.csv"), "id\n1\n2\n").unwrap();
    std::fs::write(dir.path().join("zones.csv"), "id\n9\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("taxi", dir.path()))
        .await
        .unwrap();

    // A schema that *is* a bare identifier needs nothing.
    let (_, rows) = query(&state, "user:taxi", "select * from main.zones")
        .await
        .expect("main.zones");
    assert_eq!(rows.len(), 1);

    // Quoted, the digit-named one works too.
    let (_, rows) = query(&state, "user:taxi", "select * from \"2022\".trips")
        .await
        .expect("\"2022\".trips");
    assert_eq!(rows.len(), 2);

    // Unquoted it cannot: `2022` is a number. The error has to say which schemas
    // need the quotes, because DuckDB's own message is only "syntax error".
    let Err(error) = query(&state, "user:taxi", "select * from 2022.trips").await else {
        panic!("`2022.trips` is not valid SQL");
    };
    let error = error.to_string();
    assert!(error.contains("\"2022\""), "name the schema: {error}");
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
        .register(folder_source("data", dir.path()))
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
    let target = dir
        .path()
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
    assert!(dir.path().join("out.parquet").is_file());
}

/// Pointing a source at one file must not hand over its neighbours.
#[tokio::test]
async fn a_single_file_source_exposes_only_that_file() {
    let dir = fixture();
    let state = AppState::new();
    state
        .register(folder_source("one", &dir.path().join("customers.csv")))
        .await
        .unwrap();

    let connection = state.open("user:one", None).await.unwrap();
    let tables = connection.list_tables("memory").await.unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].schema, "main");
    assert_eq!(tables[0].name, "customers");

    let (_, rows) = query(&state, "user:one", "select count(*) from customers")
        .await
        .unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(3));

    // The sibling sits in the same directory and is still out of reach: the
    // session is granted the file, not the folder around it.
    let sibling = dir
        .path()
        .join("sales/orders.csv")
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
    std::fs::write(dir.path().join("budget.csv"), "a,b\n1,2\n3,4\r\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("data", dir.path()))
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

    // And it costs you that file only — the rest of the folder still works.
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
        .register(folder_source("notes", &dir.path().join("readme.md")))
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

/// A folder source is a source, so `@import` reaches it like any other — which
/// is what makes "join a database table to a parquet file" work without a single
/// line of federation code that knows about folders.
#[tokio::test]
async fn a_folder_source_can_be_imported_into_a_federated_query() {
    let dir = fixture();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(
        other.path().join("targets.csv"),
        "name,target\nada,500\ngrace,900\n",
    )
    .unwrap();

    let state = AppState::new();
    state
        .register(folder_source("sales", dir.path()))
        .await
        .unwrap();
    state
        .register(folder_source("budget", other.path()))
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
    std::fs::create_dir_all(dir.path().join("2022")).unwrap();
    std::fs::write(dir.path().join("2022/jan.csv"), "id,total\n1,10\n2,20\n").unwrap();
    std::fs::write(dir.path().join("2022/feb.csv"), "id,total\n3,30\n").unwrap();
    std::fs::write(dir.path().join("other.csv"), "id,total\n9,90\n").unwrap();

    let state = AppState::new();
    state
        .register(folder_source("trips", dir.path()))
        .await
        .unwrap();

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
        .register(folder_source("data", dir.path()))
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
        .register(folder_source("data", dir.path()))
        .await
        .unwrap();

    let raw = std::fs::read_to_string(config.path().join("sources.json")).unwrap();
    assert!(raw.contains("\"kind\": \"files\""), "{raw}");
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
