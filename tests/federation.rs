//! Federated queries against the live demo schema.
//!
//! Skipped unless `ALKYON_SOURCES` is set — see `live.rs`.

mod common;

use alkyon::error::Result;
use alkyon::federation::{self, Limits};
use alkyon::model::RowBatch;
use alkyon::state::AppState;
use common::seeded;
use futures::StreamExt;
use serde_json::Value;

/// Run a federated buffer and collect it whole.
async fn run(state: &AppState, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    run_with(state, sql, Limits::default()).await
}

async fn run_with(
    state: &AppState,
    sql: &str,
    limits: Limits,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let program = federation::program::parse(sql)?;
    let mut batches = federation::execute(state, program, limits);

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
async fn joins_postgres_to_sql_server() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    // Each import is written in its own dialect — `top` is T-SQL, `limit` is not.
    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @import pg = pg-dev/alkyon_demo : select id, name, credit from sales.customer where id <= 5 order by id\n\
         -- @import ms = mssql-dev/alkyon_demo : select top 5 id, name, credit from sales.customer order by id\n\
         \n\
         select p.id, p.name, p.credit as pg_credit, m.credit as ms_credit\n\
         from pg p join ms m on m.id = p.id\n\
         order by p.id;",
    )
    .await
    .expect("a federated join");

    assert_eq!(columns, ["id", "name", "pg_credit", "ms_credit"]);
    assert_eq!(rows.len(), 5, "five customers from each side");

    // The same seed on both engines, so the two columns must agree exactly. This
    // is what proves the typed materialisation works: `numeric(18,2)` and
    // `decimal(18,2)` travel as strings and land as the same DECIMAL.
    for row in &rows {
        assert_eq!(row[2], row[3], "pg and sql server disagree: {row:?}");
        assert_ne!(row[2], Value::Null);
    }
}

#[tokio::test]
async fn an_imported_decimal_is_a_number_not_text() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    // `sum` over a VARCHAR would fail outright, which is the point of the test.
    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @import c = pg-dev/alkyon_demo : select credit from sales.customer\n\
         select count(*) as n, sum(credit) as total, typeof(credit) as kind from c group by kind;",
    )
    .await
    .expect("aggregating an imported decimal");

    assert_eq!(columns, ["n", "total", "kind"]);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0][2]
            .as_str()
            .is_some_and(|kind| kind.starts_with("DECIMAL")),
        "credit should have landed as DECIMAL, got {:?}",
        rows[0][2]
    );
}

#[tokio::test]
async fn integers_and_dates_survive_the_round_trip() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @import o = pg-dev/alkyon_demo : select order_id, line_no, shipped_on from sales.order_line\n\
         select typeof(order_id), typeof(line_no), typeof(shipped_on),\n\
                count(shipped_on) as shipped, count(*) - count(shipped_on) as missing\n\
         from o group by 1, 2, 3;",
    )
    .await
    .expect("typing integers and dates");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_str(), Some("BIGINT"));
    assert_eq!(rows[0][1].as_str(), Some("BIGINT"));
    assert_eq!(rows[0][2].as_str(), Some("DATE"));
    // The seed leaves a quarter of `shipped_on` NULL; NULL must stay NULL rather
    // than becoming the string "null".
    assert!(
        rows[0][4].as_i64().is_some_and(|missing| missing > 0),
        "NULLs were lost: {:?}",
        rows[0]
    );
}

#[tokio::test]
async fn the_import_cap_fails_loudly() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    // Passed in, not set through the environment: cargo runs these tests as
    // threads of one process, so a global would leak into every other test.
    let error = run_with(
        &state,
        "-- @duckdb\n\
         -- @import o = pg-dev/alkyon_demo : select * from sales.order_line\n\
         select count(*) from o;",
        Limits {
            max_import_rows: 100,
        },
    )
    .await
    .expect_err("1500 rows against a cap of 100");

    let message = error.to_string();
    assert!(message.contains("more than 100 rows"), "{message}");
    assert!(
        message.contains("ALKYON_IMPORT_MAX_ROWS"),
        "the message should say how to raise it: {message}"
    );
}

#[tokio::test]
async fn a_federated_session_cannot_read_the_filesystem() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    // No folder is open in these tests, so DuckDB gets no file access at all.
    for attempt in [
        "select * from read_csv('C:/Windows/win.ini')",
        "select * from 'C:/Windows/win.ini'",
        "copy (select 1 as x) to 'C:/Windows/Temp/alkyon-should-not-exist.csv'",
        "install httpfs",
        "set allowed_directories = ['C:/']",
    ] {
        let sql = format!("-- @duckdb\n{attempt};");
        assert!(
            run(&state, &sql).await.is_err(),
            "`{attempt}` should have been refused"
        );
    }
}

/// Files in the open folder, addressed by relative path — and export back out.
#[tokio::test]
async fn reads_and_writes_files_in_the_open_folder() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let folder = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(folder.path().join("data")).unwrap();
    std::fs::write(
        folder.path().join("data/budget.csv"),
        "customer_id,target\n1,1000.50\n2,2000.25\n3,3000.75\n",
    )
    .unwrap();
    state
        .open_workspace(folder.path().to_str().unwrap())
        .await
        .unwrap();

    // A database import joined to a CSV sitting in the folder. `${folder}` is how
    // every file is addressed — a relative path is refused by the confinement.
    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @import c = pg-dev/alkyon_demo : select id, name, credit from sales.customer where id <= 3\n\
         select c.id, c.name, b.target\n\
         from c left join '${folder}/data/budget.csv' b on b.customer_id = c.id\n\
         order by c.id;",
    )
    .await
    .expect("joining a database import to a CSV in the folder");

    assert_eq!(columns, ["id", "name", "target"]);
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|row| row[2] != Value::Null),
        "every customer should have matched a budget row: {rows:?}"
    );

    // Export to parquet, then read it back — the `COPY TO` half of the ask.
    //
    // A write names the folder explicitly: `file_search_path` only governs reads,
    // so a bare relative path here would land next to the alkyon process instead.
    run(
        &state,
        "-- @duckdb\n\
         -- @import c = pg-dev/alkyon_demo : select id, name, credit from sales.customer where id <= 10\n\
         copy (select * from c) to '${folder}/data/customers.parquet' (format parquet);",
    )
    .await
    .expect("exporting to parquet");

    assert!(
        folder.path().join("data/customers.parquet").is_file(),
        "the parquet file should exist on disk"
    );

    let (_, rows) = run(
        &state,
        "-- @duckdb\nselect count(*) as n, typeof(credit) as kind from '${folder}/data/customers.parquet' group by kind;",
    )
    .await
    .expect("reading the parquet back");
    assert_eq!(rows[0][0].as_i64(), Some(10));
    assert!(
        rows[0][1]
            .as_str()
            .is_some_and(|k| k.starts_with("DECIMAL")),
        "parquet should have kept the decimal type: {:?}",
        rows[0][1]
    );

    // Still confined: the folder is allowed, everything else is not. This is the
    // assertion that caught `allowed_directories` being an exception list rather
    // than a restriction — on its own it enforced nothing.
    for outside in [
        "select * from read_csv('C:/Windows/win.ini')",
        "select * from 'C:/Windows/win.ini'",
        "select * from read_csv('${folder}/../escape.csv')",
        "copy (select 1) to 'C:/Windows/Temp/alkyon-nope.csv'",
    ] {
        let sql = format!("-- @duckdb\n{outside};");
        assert!(
            run(&state, &sql).await.is_err(),
            "`{outside}` should have been refused"
        );
    }
}

#[tokio::test]
async fn a_plain_buffer_is_not_federated() {
    // No `-- @duckdb`, so nothing here reaches DuckDB at all.
    assert!(!federation::program::is_federated(
        "select * from sales.customer"
    ));
    assert!(federation::program::is_federated("-- @duckdb\nselect 1"));
}
