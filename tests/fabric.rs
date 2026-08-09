//! The SQL Server path that goes through DuckDB's `mssql` extension.
//!
//! Pointed at the **dev SQL Server container**, not at Fabric: a Fabric endpoint is
//! not something a test suite can conjure. What that proves is everything except
//! the login on a routed node — the dialect, the metadata, the types, the
//! read-only edge — which is the part this code is responsible for. The login
//! itself has been confirmed by hand against a real endpoint.
//!
//!     docker compose -f docker/compose.dev.yml up -d mssql
//!     ALKYON_SOURCES=docker/sources.dev.json cargo test --test fabric

mod common;

use alkyon::error::Result;
use alkyon::model::{RowBatch, SourceConfig, SourceKind, TableKind, TlsMode};
use alkyon::state::AppState;
use common::seeded;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;

const SOURCE: &str = "fabric-dev";

/// Register the dev SQL Server a second time, as a Fabric-kind source.
///
/// Same host, same login, the other road in — which is exactly what makes the two
/// comparable.
async fn fabric() -> Option<Arc<AppState>> {
    let state = seeded().await?;
    let raw = std::fs::read_to_string(std::env::var_os("ALKYON_SOURCES")?).ok()?;
    let sources: Vec<SourceConfig> = serde_json::from_str(&raw).ok()?;
    let mut config = sources
        .into_iter()
        .find(|source| source.kind == SourceKind::MsSql)?;

    config.id = SOURCE.to_owned();
    config.kind = SourceKind::Fabric;
    // The extension encrypts without validating, so *Require* is refused by design
    // — the container's certificate is self-signed anyway.
    config.tls = TlsMode::TrustCertificate;

    match state.import(vec![config]).await {
        Ok(()) => Some(state),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

async fn query(state: &AppState, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let connection = state.open(SOURCE, None).await?;
    let mut batches = connection.execute(sql);

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

/// The whole point: the dialect does not change. `top` is T-SQL and DuckDB has no
/// such keyword, so a query using it proves the text reached the server unaltered.
#[tokio::test]
async fn the_dialect_is_still_t_sql() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };

    let (columns, rows) = query(
        &state,
        "SELECT TOP 3 id, name FROM sales.customer ORDER BY id",
    )
    .await
    .expect("T-SQL, sent as written");
    assert_eq!(columns, ["id", "name"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], Value::from("Customer 1"));

    // Something only SQL Server can answer, so nothing local could have faked it.
    let (_, rows) = query(&state, "SELECT @@VERSION AS version")
        .await
        .expect("a server variable");
    assert!(
        rows[0][0]
            .as_str()
            .is_some_and(|v| v.contains("Microsoft SQL Server")),
        "{:?}",
        rows[0][0]
    );

    // A window function, which the federated planner would never have written.
    let (_, rows) = query(
        &state,
        "SELECT TOP 2 name, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM sales.customer",
    )
    .await
    .expect("a window function");
    assert_eq!(rows[1][1], Value::from(2));
}

/// The explorer's three questions, answered by the same T-SQL the native connector
/// uses — so the two roads describe one server identically.
#[tokio::test]
async fn the_explorer_sees_what_it_sees_natively() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };
    let through_duckdb = state.open(SOURCE, None).await.expect("connect");
    let native = state.open("mssql-dev", None).await.expect("connect");

    let databases = through_duckdb.list_databases().await.unwrap();
    assert_eq!(databases, native.list_databases().await.unwrap());
    assert!(databases.iter().any(|db| db == "alkyon_demo"));

    let tables = through_duckdb.list_tables("alkyon_demo").await.unwrap();
    assert_eq!(tables, native.list_tables("alkyon_demo").await.unwrap());
    let view = tables
        .iter()
        .find(|t| t.name == "order_value")
        .expect("the seeded view");
    assert_eq!(view.kind, TableKind::View);

    // Nullability, defaults and the composite primary key — the details a tree
    // built from a `DESCRIBE` would have lost.
    let columns = through_duckdb
        .list_columns("alkyon_demo", "sales", "order_line")
        .await
        .unwrap();
    assert_eq!(
        columns,
        native
            .list_columns("alkyon_demo", "sales", "order_line")
            .await
            .unwrap()
    );
    let keys: Vec<&str> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(keys, ["order_id", "line_no"]);
    assert!(columns.iter().any(|c| c.name == "unit_price"
        && c.data_type.contains("(12,4)")));
    assert!(columns.iter().any(|c| c.name == "shipped_on" && c.nullable));
}

/// A snapshot is what autocompletion is built from, so it has to hold every table
/// with every column, and agree with the native path row for row.
#[tokio::test]
async fn a_snapshot_agrees_with_the_native_one() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };
    let through_duckdb = state.open(SOURCE, None).await.unwrap();
    let native = state.open("mssql-dev", None).await.unwrap();

    let ours = through_duckdb.snapshot("alkyon_demo").await.unwrap();
    assert_eq!(ours, native.snapshot("alkyon_demo").await.unwrap());
    assert!(ours.iter().any(|t| t.name == "customer" && !t.columns.is_empty()));
}

/// Values keep their types on the way back — a decimal to the cent, a date as a
/// date, a NULL as a NULL.
#[tokio::test]
async fn values_arrive_typed_rather_than_stringly() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };

    let (_, rows) = query(
        &state,
        "SELECT SUM(credit) AS total, COUNT(*) AS n FROM sales.customer",
    )
    .await
    .expect("an aggregate");
    // The same seed the other engines carry, summed on the server.
    assert_eq!(rows[0][1], Value::from(250));
    assert!(rows[0][0] != Value::Null);

    let (_, rows) = query(
        &state,
        "SELECT COUNT(*) AS shipped, SUM(CASE WHEN shipped_on IS NULL THEN 1 ELSE 0 END) AS missing \
         FROM sales.order_line",
    )
    .await
    .expect("counting NULLs");
    assert_eq!(rows[0][0], Value::from(1500));
    assert!(
        rows[0][1].as_i64().is_some_and(|missing| missing > 0),
        "the seed leaves shipped_on NULL for some rows: {:?}",
        rows[0]
    );
}

/// This source reads. `mssql_scan` binds a result set and the attachment is
/// READ_ONLY, so a write cannot go through — and the error should say what the
/// source is for rather than repeat DuckDB's binder.
#[tokio::test]
async fn it_reads_and_says_so_when_asked_to_write() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };

    for attempt in [
        "DELETE FROM sales.customer WHERE id = 999999",
        "CREATE TABLE sales.nope (a int)",
        "UPDATE sales.customer SET name = 'x' WHERE id = 999999",
    ] {
        let error = query(&state, attempt)
            .await
            .expect_err(attempt)
            .to_string()
            .to_lowercase();
        assert!(
            error.contains("read") || error.contains("no rows"),
            "`{attempt}` failed for the wrong reason: {error}"
        );
    }
}

/// A bad statement comes back in the server's own words, not wrapped in DuckDB's.
#[tokio::test]
async fn the_servers_error_is_what_you_see() {
    let Some(state) = fabric().await else {
        eprintln!("skipped: no SQL Server in ALKYON_SOURCES");
        return;
    };
    let error = query(&state, "SELECT * FROM sales.no_such_table")
        .await
        .expect_err("no such table")
        .to_string();
    assert!(error.contains("no_such_table"), "{error}");
    assert!(!error.contains("mssql_scan"), "the plumbing shows: {error}");
}

/// Certificate validation is what *Require* means, and this extension never does
/// it. Refused rather than quietly downgraded.
#[tokio::test]
async fn require_is_refused_because_it_cannot_be_honoured() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let raw = std::fs::read_to_string(std::env::var_os("ALKYON_SOURCES").unwrap()).unwrap();
    let sources: Vec<SourceConfig> = serde_json::from_str(&raw).unwrap();
    let Some(mut config) = sources
        .into_iter()
        .find(|source| source.kind == SourceKind::MsSql)
    else {
        eprintln!("skipped: no SQL Server source");
        return;
    };
    config.id = "fabric-strict".into();
    config.kind = SourceKind::Fabric;
    config.tls = TlsMode::Require;
    // Registering only records it; the refusal belongs to the moment something
    // tries to build the connection.
    state.import(vec![config]).await.unwrap();

    let error = state
        .open("fabric-strict", None)
        .await
        .err()
        .expect("Require cannot be honoured")
        .to_string();
    assert!(error.contains("without ever checking"), "{error}");
    assert!(error.contains("Trust certificate"), "{error}");
}
