//! Tests against real servers, driving the `Connector`/`Connection` traits
//! directly — the backend is meant to be exercisable before any UI exists.
//!
//! They are skipped unless `ALKYON_SOURCES` points at a source file, and they
//! expect the demo schema from `docker/compose.dev.yml`:
//!
//! ```text
//! docker compose -f docker/compose.dev.yml up -d
//! $env:ALKYON_SOURCES = "docker/sources.dev.json"; cargo test
//! ```

mod common;

use alkyon::model::{RowBatch, TableKind};
use alkyon::state::AppState;
use common::{relational, seeded};
use futures::StreamExt;
use serde_json::Value;

/// Collect a whole result set: the column names, and every row in order.
async fn collect(state: &AppState, id: &str, sql: &str) -> (Vec<String>, Vec<Vec<Value>>, usize) {
    let connection = state.open(id, None).await.expect("connect");
    let mut batches = connection.execute(sql);

    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut row_batches = 0;

    while let Some(batch) = batches.next().await {
        match batch.expect("batch") {
            RowBatch::Columns(meta) => {
                columns = meta.iter().map(|c| c.name.clone()).collect();
            }
            RowBatch::Rows(batch) => {
                row_batches += 1;
                rows.extend(batch);
            }
            RowBatch::Affected(_) => {}
        }
    }
    (columns, rows, row_batches)
}

#[tokio::test]
async fn metadata_describes_the_demo_schema() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    for source in state.summaries().await {
        let id = &source.id;
        let connection = state.open(id, None).await.expect("connect");

        // Every kind answers these two, whatever its seed looks like.
        let databases = connection.list_databases().await.expect("list_databases");
        assert!(
            databases.contains(&source.database),
            "{id}: {databases:?} should contain {}",
            source.database
        );

        let tables = connection
            .list_tables(&source.database)
            .await
            .expect("list_tables");
        assert!(!tables.is_empty(), "{id}: no tables at all");
        if !relational(source.kind) {
            continue;
        }

        let find = |name: &str| {
            tables
                .iter()
                .find(|t| t.schema == "sales" && t.name == name)
                .unwrap_or_else(|| panic!("{id}: no sales.{name} in {tables:?}"))
        };
        assert_eq!(find("customer").kind, TableKind::Table);
        assert_eq!(find("order_line").kind, TableKind::Table);
        assert_eq!(find("order_value").kind, TableKind::View);

        let columns = connection
            .list_columns(&source.database, "sales", "order_line")
            .await
            .expect("list_columns");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "order_id",
                "line_no",
                "customer_id",
                "sku",
                "quantity",
                "unit_price",
                "shipped_on",
                "signature"
            ],
            "{id}: columns should come back in ordinal order"
        );

        let primary_key: Vec<&str> = columns
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(primary_key, ["order_id", "line_no"], "{id}: composite key");

        let shipped_on = columns.iter().find(|c| c.name == "shipped_on").unwrap();
        assert!(shipped_on.nullable, "{id}: shipped_on is nullable");
        assert_eq!(shipped_on.data_type, "date", "{id}: engine type spelling");

        let unit_price = columns.iter().find(|c| c.name == "unit_price").unwrap();
        assert!(
            unit_price.data_type.contains("(12,4)"),
            "{id}: precision and scale should survive, got {}",
            unit_price.data_type
        );
    }
}

#[tokio::test]
async fn queries_stream_in_batches() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    for source in state.summaries().await {
        if !relational(source.kind) {
            continue;
        }
        let id = &source.id;
        let (columns, rows, batches) = collect(
            &state,
            id,
            "SELECT order_id, line_no, unit_price, shipped_on FROM sales.order_line ORDER BY order_id, line_no",
        )
        .await;

        assert_eq!(columns, ["order_id", "line_no", "unit_price", "shipped_on"]);
        assert_eq!(rows.len(), 1500, "{id}: every row arrives");
        assert!(
            batches >= 3,
            "{id}: 1500 rows should not arrive as {batches} batch(es)"
        );

        // Fixed-scale decimals keep their digits by travelling as strings.
        assert_eq!(rows[0][2], Value::String("9.9900".into()), "{id}: numeric");

        // NULLs are JSON null, not an empty string.
        let nulls = rows.iter().filter(|r| r[3] == Value::Null).count();
        assert!(nulls > 0, "{id}: shipped_on has NULLs in the seed data");
    }
}

/// Every column of the demo `customer` table decodes to something real.
///
/// The narrow version of this test — pick four columns and check them — is what
/// let a `char(2)` render as `<decode error>` for months: sqlx names `char(n)`
/// "CHAR" and Postgres's internal one-byte `"char"` "\"CHAR\"", and the two were
/// mapped the wrong way round. Selecting `*` is the point.
#[tokio::test]
async fn every_column_of_the_demo_table_decodes() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    for source in state.summaries().await {
        if !relational(source.kind) {
            continue;
        }
        let id = &source.id;
        let (columns, rows, _) = collect(&state, id, "SELECT * FROM sales.customer").await;
        assert!(!rows.is_empty(), "{id}: the seed has customers");

        for (index, name) in columns.iter().enumerate() {
            for row in &rows {
                let cell = &row[index];
                let text = cell.as_str().unwrap_or_default();
                assert!(
                    !text.starts_with("<decode error") && !text.starts_with("<unsupported type"),
                    "{id}: column `{name}` did not decode — {cell}"
                );
            }
        }

        // The one that was actually broken, spelled out so a regression names
        // itself rather than hiding in the loop above.
        let country = columns
            .iter()
            .position(|c| c == "country")
            .expect("country");
        assert!(
            rows.iter()
                .any(|row| row[country].as_str().is_some_and(|v| v.trim().len() == 2)),
            "{id}: char(2) should come back as its two characters"
        );
    }
}

#[tokio::test]
async fn unknown_source_is_reported() {
    let state = AppState::new();
    let Err(error) = state.open("nope", None).await else {
        panic!("opening an unregistered source should fail");
    };
    assert_eq!(error.to_string(), "unknown source `nope`");
}
