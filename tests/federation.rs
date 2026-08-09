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

/// There is no row cap. The old default was a million, and it was really a memory
/// limit wearing a row count — every cell was held as a `String` in this process
/// until the import finished.
///
/// So the number here is deliberately just past that million: it is the assertion
/// that the old ceiling is gone, and it only passes because the rows now go into
/// DuckDB as they arrive.
#[tokio::test]
async fn an_import_is_not_capped() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @import big = pg-dev/alkyon_demo : select i, i * 2 as double from generate_series(1, 1200000) g(i)\n\
         select count(*) as n, sum(double) as total, max(i) as biggest from big;",
    )
    .await
    .expect("an import past the old million-row ceiling");

    assert_eq!(rows[0][0], Value::from(1_200_000));
    assert_eq!(rows[0][2], Value::from(1_200_000));
    // 2 × (1 + 2 + … + 1 200 000), exactly — a float would have drifted.
    assert_eq!(rows[0][1], Value::from(1_440_001_200_000i64));
}

/// A cap is opt-in, and when one is asked for it still fails rather than
/// truncating: a join quietly missing half its rows is worse than a query that
/// failed.
#[tokio::test]
async fn an_opt_in_cap_still_fails_loudly() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    assert_eq!(
        Limits::default().max_import_rows,
        None,
        "the default must be no cap at all"
    );

    // Passed in, not set through the environment: cargo runs these tests as
    // threads of one process, so a global would leak into every other test.
    let error = run_with(
        &state,
        "-- @duckdb\n\
         -- @import o = pg-dev/alkyon_demo : select * from sales.order_line\n\
         select count(*) from o;",
        Limits {
            max_import_rows: Some(100),
        },
    )
    .await
    .expect_err("1500 rows against a cap of 100");

    let message = error.to_string();
    assert!(message.contains("more than 100 rows"), "{message}");
    assert!(
        message.contains("ALKYON_IMPORT_MAX_ROWS"),
        "the message should say where the ceiling came from: {message}"
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

// ------------------------------------------------------------------ @attach

/// The physical plan of `sql` against an attached PostgreSQL, as one blob of text.
async fn plan_of(state: &AppState, sql: &str) -> String {
    let buffer = format!("-- @duckdb\n-- @attach pg = pg-dev/alkyon_demo\nEXPLAIN {sql};");
    let (_, rows) = run(state, &buffer).await.expect("a plan");
    rows.iter()
        .filter_map(|row| row.last().and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn an_attached_server_is_queried_where_it_lives() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    // The alias is a *catalogue*, so the name has three parts. Nothing was read
    // until this query asked.
    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach pg = pg-dev/alkyon_demo\n\
         select count(*) as n from pg.sales.customer;",
    )
    .await
    .expect("an attached postgres answers");
    assert_eq!(columns, ["n"]);
    assert_eq!(rows[0][0], Value::from(250));

    // A view on the remote side is a relation like any other.
    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach pg = pg-dev/alkyon_demo\n\
         select count(*) as n from pg.sales.order_value;",
    )
    .await
    .expect("a remote view");
    assert!(rows[0][0].as_i64().is_some_and(|n| n > 0), "{:?}", rows[0]);
}

#[tokio::test]
async fn mysql_attaches_too() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    // MySQL has no schema layer, so the name has two parts rather than three.
    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach my = mysql-dev/sales\n\
         select count(*) as n from my.customer;",
    )
    .await
    .expect("an attached mysql answers");
    assert!(rows[0][0].as_i64().is_some_and(|n| n > 0), "{:?}", rows[0]);
}

/// What `@attach` buys, and what it does not — asserted rather than promised.
///
/// The plan is the evidence: a filter lands *inside* `POSTGRES_SCAN`, so the server
/// applies it, while a `group by` sits above the scan, so DuckDB does the grouping
/// after every value has crossed the wire. That second half is the reason `@import`
/// is still the right tool when the remote engine should do the work.
#[tokio::test]
async fn a_filter_is_pushed_down_but_a_group_by_is_not() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let filtered = plan_of(&state, "select name from pg.sales.customer where id = 7").await;
    assert!(filtered.contains("POSTGRES_SCAN"), "{filtered}");
    assert!(
        filtered.contains("Filters: id=7"),
        "the filter should have gone to the server: {filtered}"
    );
    assert!(
        filtered.contains("Projections:"),
        "only the named columns should be read: {filtered}"
    );

    let grouped = plan_of(&state, "select country, count(*) from pg.sales.customer group by 1").await;
    let scan = grouped.find("POSTGRES_SCAN").expect("a scan");
    let group = grouped.find("HASH_GROUP_BY").expect("a local group-by");
    assert!(
        group < scan,
        "the group-by sits above the scan, so it happens here and not there:\n{grouped}"
    );
}

/// An attached catalogue joined to an imported table: the two ways in, in one
/// buffer, which is the arrangement that makes them complementary rather than
/// competing.
#[tokio::test]
async fn an_attached_server_joins_an_imported_one() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach pg = pg-dev/alkyon_demo\n\
         -- @import ms = mssql-dev/alkyon_demo : select top 5 id, credit from sales.customer order by id\n\
         select p.id, p.name, m.credit\n\
         from pg.sales.customer p join ms m on m.id = p.id\n\
         order by p.id;",
    )
    .await
    .expect("joining an attached server to an imported one");

    assert_eq!(columns, ["id", "name", "credit"]);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row[2] != Value::Null), "{rows:?}");
}

/// `@attach` is READ_ONLY without an opt-out, because it is the only path in the
/// whole program that could otherwise write to a production server.
#[tokio::test]
async fn an_attachment_cannot_be_written_to() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    for attempt in [
        "insert into pg.sales.customer (name) values ('nope')",
        "delete from pg.sales.customer where id = 1",
        "create table pg.sales.nope (a int)",
    ] {
        let sql = format!("-- @duckdb\n-- @attach pg = pg-dev/alkyon_demo\n{attempt};");
        let error = run(&state, &sql)
            .await
            .expect_err(&format!("`{attempt}` should have been refused"));
        assert!(
            error.to_string().to_lowercase().contains("read-only")
                || error.to_string().to_lowercase().contains("read only"),
            "`{attempt}` failed for the wrong reason: {error}"
        );
    }
}

/// Attaching opens the network, and the measurement that made this design
/// possible is that the door can be shut afterwards. This is that measurement, as
/// a test: a session with a live server in it must still be unable to read the
/// machine it runs on.
#[tokio::test]
async fn an_attached_session_is_still_confined() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    for attempt in [
        "select * from read_csv('C:/Windows/win.ini')",
        "select * from 'C:/Windows/win.ini'",
        "copy (select 1 as x) to 'C:/Windows/Temp/alkyon-should-not-exist.csv'",
        "install httpfs",
        // The extension is loaded, so this is the interesting one: a second
        // attachment of the user's own choosing, to a server alkyon never approved.
        "attach 'host=127.0.0.1 port=55432 dbname=alkyon_demo user=postgres password=alkyon-dev' as sneaky (type postgres)",
    ] {
        let sql = format!("-- @duckdb\n-- @attach pg = pg-dev/alkyon_demo\n{attempt};");
        assert!(
            run(&state, &sql).await.is_err(),
            "`{attempt}` should have been refused"
        );
    }

    // And the attachment itself still works, so the confinement did not simply
    // break everything.
    let (_, rows) = run(
        &state,
        "-- @duckdb\n-- @attach pg = pg-dev/alkyon_demo\nselect count(*) as n from pg.sales.customer;",
    )
    .await
    .expect("the attachment survives the confinement");
    assert_eq!(rows[0][0], Value::from(250));
}

/// The question a community extension raises: alkyon loaded third-party native code
/// into this process, so can a *query* now load some of its own?
///
/// It must not. Alkyon decides what is loaded, before the session is frozen; after
/// that, `LOAD` is refused outright — including for the very extension already in
/// the session, and including any attempt to reopen the door.
#[tokio::test]
async fn a_query_cannot_load_native_code_of_its_own() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    for attempt in [
        // A *different* extension — the one that would actually be new code.
        "load postgres",
        "install postgres",
        "install mssql from community",
        "set allow_community_extensions = true",
    ] {
        // In a session that already has the community extension loaded, which is
        // the permissive case.
        let sql = format!("-- @duckdb\n-- @attach ms = mssql-dev/alkyon_demo\n{attempt};");
        assert!(
            run(&state, &sql).await.is_err(),
            "`{attempt}` should have been refused"
        );
    }

    // `load mssql` is the exception, and it is not a hole: the extension is already
    // in the process, so re-issuing the statement loads nothing. What matters is
    // that nothing *new* can arrive, which is what the list above asserts.
    let already = run(
        &state,
        "-- @duckdb\n-- @attach ms = mssql-dev/alkyon_demo\nload mssql;",
    )
    .await;
    assert!(already.is_ok(), "{already:?}");

    // And in a session that asked for nothing, even that is refused.
    assert!(run(&state, "-- @duckdb\nload mssql;").await.is_err());
}

/// **A gap, pinned so it cannot be forgotten**: once the community `mssql`
/// extension is in the session, a buffer's own `ATTACH` reaches the network even
/// though external access is off.
///
/// The core `postgres` extension refuses the same thing — `an_attached_session_is_still_confined`
/// asserts that — so this is a difference between DuckDB's own code and a
/// third party's, not something alkyon chose. Files stay shut either way; it is
/// outbound connections that leak.
///
/// It is written down rather than papered over because it changes what a federated
/// buffer is: with this extension loaded, a `.sql` file someone sends you can open
/// a connection alkyon never approved. `ALKYON_COMMUNITY_EXTENSIONS=off` is the way
/// back. The test asserts today's behaviour so that the day it starts being refused,
/// this fails and the guide gets corrected.
#[tokio::test]
async fn a_community_extension_lets_a_buffer_reach_the_network_itself() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let sneaky = run(
        &state,
        "-- @duckdb\n\
         -- @attach ms = mssql-dev/alkyon_demo\n\
         attach 'mssql://sa:Alkyon-dev-1@127.0.0.1:51433?database=alkyon_demo' as sneaky (type mssql);",
    )
    .await;
    assert!(
        sneaky.is_ok(),
        "if this now fails, the extension started honouring enable_external_access \
         — delete this test and the caveat in the guide: {sneaky:?}"
    );

    // The disk stays shut, which is the half that did hold.
    assert!(run(
        &state,
        "-- @duckdb\n\
         -- @attach ms = mssql-dev/alkyon_demo\n\
         select * from read_csv('C:/Windows/win.ini');",
    )
    .await
    .is_err());
}

/// SQL Server, through a **community** extension — third-party code, fetched
/// because the buffer asked for it.
///
/// Worth its own test beyond "it answers": this is the engine whose native path
/// cannot reach a Fabric SQL endpoint, so the federated one carrying a bearer token
/// is not a convenience but a way in.
#[tokio::test]
async fn sql_server_attaches_through_the_community_extension() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let (columns, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach ms = mssql-dev/alkyon_demo\n\
         select count(*) as n from ms.sales.customer;",
    )
    .await
    .expect("an attached sql server answers");
    assert_eq!(columns, ["n"]);
    assert_eq!(rows[0][0], Value::from(250));

    // A remote view, and a date that stayed a date rather than becoming text.
    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach ms = mssql-dev/alkyon_demo\n\
         select typeof(shipped_on) as kind from ms.sales.order_line limit 1;",
    )
    .await
    .expect("types survive the attachment");
    assert_eq!(rows[0][0], Value::from("DATE"));
}

/// The filter reaches SQL Server. Its plan prints no `Filters:` line — unlike
/// PostgreSQL's — so the only thing that settles it is how many rows the scan
/// actually produced.
#[tokio::test]
async fn a_filter_reaches_sql_server_even_though_the_plan_is_quiet_about_it() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let (_, rows) = run(
        &state,
        "-- @duckdb\n\
         -- @attach ms = mssql-dev/alkyon_demo\n\
         explain analyze select count(*) from ms.sales.customer where country = 'BE';",
    )
    .await
    .expect("an analysed plan");
    let plan = rows
        .iter()
        .filter_map(|row| row.last().and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plan.contains("MSSQL_CATALOG_SCAN"), "{plan}");
    // The seed has 83 Belgian customers out of 250. If the scan reports 250, the
    // filter was applied here and every row crossed the wire for nothing.
    assert!(
        plan.contains("83 rows"),
        "the scan should have produced only the matching rows:\n{plan}"
    );
    assert!(!plan.contains("250 rows"), "{plan}");
}

/// `Require` means *validate the certificate*, and this extension never does —
/// measured: no secret or DSN parameter changes it, and a certificate that cannot
/// match the host is accepted anyway. So it is refused rather than downgraded.
#[tokio::test]
async fn sql_server_refuses_require_rather_than_quietly_weakening_it() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let mut config: Vec<alkyon::model::SourceConfig> = serde_json::from_str(
        &std::fs::read_to_string(std::env::var_os("ALKYON_SOURCES").unwrap()).unwrap(),
    )
    .unwrap();
    let Some(strict) = config
        .iter_mut()
        .find(|source| source.kind == alkyon::model::SourceKind::MsSql)
    else {
        eprintln!("skipped: no SQL Server source");
        return;
    };
    strict.id = "mssql-strict".into();
    strict.tls = alkyon::model::TlsMode::Require;
    let strict = strict.clone();
    state.import(vec![strict]).await.unwrap();

    let error = run(
        &state,
        "-- @duckdb\n-- @attach ms = mssql-strict\nselect 1;",
    )
    .await
    .expect_err("Require cannot be honoured here")
    .to_string();
    assert!(error.contains("without ever checking"), "{error}");
    assert!(error.contains("@import"), "{error}");
}

/// MongoDB has a community extension, but not one published for every DuckDB build
/// — and alkyon reads MongoDB itself anyway. The error says both.
#[tokio::test]
async fn mongodb_says_why_it_cannot_be_attached() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let error = run(&state, "-- @duckdb\n-- @attach x = mongo-dev\nselect 1;")
        .await
        .expect_err("no mongo extension")
        .to_string();
    assert!(error.contains("@import"), "{error}");
    assert!(error.contains("not published"), "{error}");
}

#[tokio::test]
async fn a_plain_buffer_is_not_federated() {
    // No `-- @duckdb`, so nothing here reaches DuckDB at all.
    assert!(!federation::program::is_federated(
        "select * from sales.customer"
    ));
    assert!(federation::program::is_federated("-- @duckdb\nselect 1"));
}
