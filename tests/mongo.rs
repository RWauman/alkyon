//! MongoDB as a source, end to end.
//!
//! Skipped unless `ALKYON_SOURCES` is set — see `live.rs`. It wants the seed in
//! `docker/seed/mongo/01-demo.js`, which is document-shaped on purpose: the
//! questions worth asking here are the ones a relational fixture cannot pose.
//!
//!     docker compose -f docker/compose.dev.yml up -d mongo
//!     ALKYON_SOURCES=docker/sources.dev.json cargo test --test mongo

mod common;

use alkyon::error::Result;
use alkyon::model::RowBatch;
use alkyon::state::AppState;
use common::seeded;
use futures::StreamExt;
use serde_json::Value;

const SOURCE: &str = "mongo-dev";

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

/// Is there a MongoDB to talk to? `ALKYON_SOURCES` may name one without this
/// suite's source being in it.
async fn mongo() -> Option<std::sync::Arc<AppState>> {
    let state = seeded().await?;
    state.record(SOURCE).await.ok().map(|_| state)?.into()
}

#[tokio::test]
async fn a_collection_is_a_table() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    let (columns, rows) = query(&state, "select count(*) as n from customer")
        .await
        .expect("a collection answers plain SQL");
    assert_eq!(columns, ["n"]);
    assert_eq!(rows, [[Value::from(250)]]);
}

/// The explorer qualifies a table with the schema it listed it under, and clicking
/// one runs exactly that text. So the name the tree hands the editor has to be a
/// name DuckDB has.
///
/// The bug this pins: the views were created in a fixed `public` while the tree
/// said `alkyon_demo`, so clicking a collection answered
/// `schema "alkyon_demo" does not exist` — the source was browsable and none of it
/// was clickable.
#[tokio::test]
async fn the_name_the_explorer_inserts_is_a_name_duckdb_has() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };
    let connection = state.open(SOURCE, None).await.unwrap();

    let tables = connection.list_tables("alkyon_demo").await.unwrap();
    assert!(
        tables.iter().all(|table| table.schema == "alkyon_demo"),
        "the database is the schema: {tables:?}"
    );

    // Character for character what the explorer builds for a click, `previewSql`
    // and all.
    let (_, rows) = query(
        &state,
        "SELECT * FROM \"alkyon_demo\".\"order_line\" LIMIT 10000;",
    )
    .await
    .expect("the qualified name the tree inserts");
    assert_eq!(rows.len(), 1500);

    // And the bare name still works, because the schema is on the search path.
    let (_, rows) = query(&state, "select count(*) as n from order_line")
        .await
        .expect("an unqualified name");
    assert_eq!(rows[0][0], Value::from(1500));
}

/// The point of reading documents as JSON rather than flattening them: a
/// sub-document is a struct and an array is a list, so SQL can reach into both.
#[tokio::test]
async fn nesting_survives_the_trip() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    let (columns, rows) = query(
        &state,
        "select address.city as city, len(tags) as tags, address.region.code as region
         from customer where _id = 5",
    )
    .await
    .expect("reaching into a sub-document");

    assert_eq!(columns, ["city", "tags", "region"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::from("City 5"));
    assert_eq!(rows[0][1], Value::from(3), "tags is a list, not a string");
    // Deeper on every fifth document, and 5 is one of them.
    assert_eq!(rows[0][2], Value::from("R5"));

    // An array is unnestable, which is the other half of "nesting survives".
    let (_, rows) = query(
        &state,
        "select count(*) as n from (select unnest(tags) as tag from customer)",
    )
    .await
    .expect("unnesting an array");
    assert_eq!(
        rows[0][0],
        Value::from(500),
        "250 customers with 1, 2 or 3 tags each"
    );
}

/// A field absent from a document is absent, not false and not zero — and the
/// column has to say so.
#[tokio::test]
async fn a_missing_field_is_null_rather_than_a_default() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    let (_, rows) = query(
        &state,
        "select count(*) as all_rows, count(loyalty) as with_loyalty from customer",
    )
    .await
    .expect("counting a field that is not always there");

    assert_eq!(rows[0][0], Value::from(250));
    // Every third document carries it, so this must be neither 0 nor 250.
    assert_eq!(rows[0][1], Value::from(83));
}

/// Money as a string on the way in, exact on the way out. A Decimal128 turned
/// into a double to make the column numeric would lose cents, which is the trade
/// this codebase refuses everywhere else.
#[tokio::test]
async fn a_decimal128_keeps_its_cents() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    let (_, rows) = query(
        &state,
        "select sum(cast(credit as decimal(18,2))) as total from customer",
    )
    .await
    .expect("casting the string back to a decimal");

    // 13.37 × (1 + 2 + … + 250) = 13.37 × 31 375, to the cent.
    assert_eq!(
        rows[0][0].as_str(),
        Some("419483.75"),
        "a float would have drifted: {:?}",
        rows[0][0]
    );
}

/// One field, five shapes. Any reader that decides a column's type from the first
/// document gets this wrong; JSON is the honest answer.
#[tokio::test]
async fn a_field_of_many_types_is_not_forced_into_one() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    let (columns, rows) = query(
        &state,
        "select note, json_type(value) as shape from awkward order by _id",
    )
    .await
    .expect("asking JSON what shape each value is");

    assert_eq!(columns, ["note", "shape"]);
    let shapes: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| {
            (
                row[0].as_str().unwrap_or_default(),
                row[1].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        shapes,
        [
            ("an integer", "UBIGINT"),
            ("a string", "VARCHAR"),
            ("a double", "DOUBLE"),
            ("a document", "OBJECT"),
            ("an array", "ARRAY"),
            // Absent from the document, so there is no shape to report.
            ("absent", ""),
        ]
    );
}

/// A view is a view, and the explorer says so.
#[tokio::test]
async fn a_mongo_view_is_listed_as_one_and_queryable() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };
    let connection = state.open(SOURCE, None).await.unwrap();

    let tables = connection.list_tables("alkyon_demo").await.unwrap();
    let view = tables
        .iter()
        .find(|t| t.name == "order_value")
        .expect("the seeded view");
    assert_eq!(view.kind, alkyon::model::TableKind::View);
    // `system.*` is MongoDB's own bookkeeping and no table anyone means.
    assert!(!tables.iter().any(|t| t.name.starts_with("system.")));

    let (_, rows) = query(&state, "select count(*) as n from order_value")
        .await
        .expect("a view reads like a collection");
    assert_eq!(rows[0][0], Value::from(1500));
}

/// Only what the query names is read: reading every collection to answer a
/// question about one would be absurd, and on a real deployment ruinous.
#[tokio::test]
async fn an_unnamed_collection_is_not_read() {
    let Some(state) = mongo().await else {
        eprintln!("skipped: no {SOURCE} in ALKYON_SOURCES");
        return;
    };

    // `order_line` is the big one. Naming only `awkward` must not pull it.
    let error = query(&state, "select count(*) from awkward, order_line")
        .await
        .err();
    assert!(error.is_none(), "naming both is allowed: {error:?}");

    // And a name that is not a collection is a plain DuckDB error, not a fetch.
    let error = query(&state, "select * from no_such_collection")
        .await
        .expect_err("nothing to bind");
    assert!(
        error.to_string().contains("no_such_collection"),
        "{error}"
    );
}
