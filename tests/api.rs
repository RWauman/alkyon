//! Tests for the HTTP and WebSocket surface.
//!
//! The REST routes run through the router directly; the WebSocket ones need a
//! real socket, so they bind an ephemeral port. Everything that touches a
//! database is skipped unless `ALKYON_SOURCES` is set — see `live.rs`.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alkyon::model::SourceKind;
use alkyon::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{relational, seeded};
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

async fn get(state: Arc<AppState>, uri: &str) -> (StatusCode, String) {
    let response = alkyon::api::router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router response");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post(state: Arc<AppState>, uri: &str, body: &Value) -> (StatusCode, String) {
    let response = alkyon::api::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("router response");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get_json(state: Arc<AppState>, uri: &str) -> Value {
    let (status, body) = get(state, uri).await;
    assert_eq!(status, StatusCode::OK, "GET {uri} -> {body}");
    serde_json::from_str(&body).expect("JSON body")
}

/// Serve the router on an ephemeral port and return its address.
async fn serve(state: Arc<AppState>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, alkyon::api::router(state))
            .await
            .unwrap();
    });
    addr
}

#[tokio::test]
async fn health_and_embedded_ui_are_served() {
    let state = AppState::new();

    let health = get_json(Arc::clone(&state), "/health").await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));

    let (status, page) = get(Arc::clone(&state), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("<title>Alkyon</title>"),
        "index.html is embedded"
    );

    let (status, _) = get(Arc::clone(&state), "/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(state, "/sources/nope/databases").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, r#"{"error":"unknown source `nope`"}"#);
}

/// The source dialog's file list: registering a folder is where you say "just this
/// one file, actually", and nothing else can say what there is to choose from.
#[tokio::test]
async fn the_file_list_reports_what_a_folder_holds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sales")).unwrap();
    std::fs::write(dir.path().join("a.csv"), "x\n1\n").unwrap();
    std::fs::write(dir.path().join("notes.md"), "not data").unwrap();
    std::fs::write(dir.path().join("old.parquet"), "not really parquet").unwrap();
    std::fs::write(dir.path().join("sales/b.csv"), "x\n2\n").unwrap();

    // A Windows path is not a URI: the separators have to be escaped by hand.
    let path = dir.path().to_string_lossy().replace('\\', "%5C");
    let state = AppState::new();

    let listed = get_json(
        Arc::clone(&state),
        &format!("/files?path={path}&format=csv"),
    )
    .await;
    assert_eq!(
        listed["files"],
        json!(["a.csv", "sales/b.csv"]),
        "relative, forward-slashed, and only the declared format"
    );

    // Without a format, every readable file — which is what the dialog shows
    // before one has been chosen.
    let listed = get_json(Arc::clone(&state), &format!("/files?path={path}")).await;
    assert_eq!(
        listed["files"],
        json!(["a.csv", "old.parquet", "sales/b.csv"])
    );

    let file = dir
        .path()
        .join("a.csv")
        .to_string_lossy()
        .replace('\\', "%5C");
    let (status, body) = get(state, &format!("/files?path={file}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("is not a folder"), "{body}");
}

#[tokio::test]
async fn metadata_routes_answer_without_leaking_credentials() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };

    let sources = get_json(Arc::clone(&state), "/sources").await;
    let listed = sources.as_array().expect("array of sources");
    assert!(!listed.is_empty());
    let serialised = sources.to_string();
    // `auth_method` legitimately says "password", so look for the field and for
    // the dev credentials themselves.
    for secret in ["\"password\":", "\"token\":", "alkyon-dev", "Alkyon-dev-1"] {
        assert!(
            !serialised.contains(secret),
            "/sources leaked `{secret}`: {serialised}"
        );
    }

    for source in listed {
        let id = source["id"].as_str().unwrap();
        let db = source["database"].as_str().unwrap();

        let databases = get_json(Arc::clone(&state), &format!("/sources/{id}/databases")).await;
        assert!(databases.as_array().unwrap().contains(&json!(db)));

        let tables = get_json(Arc::clone(&state), &format!("/sources/{id}/tables?db={db}")).await;
        assert!(!tables.as_array().unwrap().is_empty(), "{id}: no tables");
        let kind: SourceKind =
            serde_json::from_value(source["kind"].clone()).expect("a known source kind");
        if !relational(kind) {
            continue;
        }
        assert!(
            tables.as_array().unwrap().iter().any(|t| {
                t["schema"] == "sales" && t["name"] == "customer" && t["kind"] == "table"
            }),
            "{id}: sales.customer missing from {tables}"
        );

        let columns = get_json(
            Arc::clone(&state),
            &format!("/sources/{id}/columns?db={db}&schema=sales&table=customer"),
        )
        .await;
        let id_column = &columns.as_array().unwrap()[0];
        assert_eq!(id_column["name"], "id", "{id}: ordinal order");
        assert_eq!(id_column["is_primary_key"], true);
        assert_eq!(id_column["nullable"], false);
    }
}

#[tokio::test]
async fn registering_a_source_connects_first() {
    let Some(path) = std::env::var_os("ALKYON_SOURCES") else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let raw = std::fs::read_to_string(&path).unwrap();
    let definitions: Vec<Value> = serde_json::from_str(&raw).unwrap();
    let state = AppState::new();

    for definition in definitions {
        let kind = definition["kind"].as_str().unwrap().to_owned();

        // A source whose credentials are wrong is not registered at all.
        let mut rejected = definition.clone();
        rejected["id"] = json!(format!("{kind}-rejected"));
        rejected["auth"]["password"] = json!("definitely-not-the-password");
        let (status, body) = post(Arc::clone(&state), "/sources", &rejected).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{kind}: {body}");

        let (status, body) = post(Arc::clone(&state), "/sources", &definition).await;
        assert_eq!(status, StatusCode::CREATED, "{kind}: {body}");
        let secret = definition["auth"]["password"].as_str().unwrap();
        assert!(
            !body.contains("\"password\":") && !body.contains(secret),
            "{kind}: 201 body leaked a secret: {body}"
        );

        // Registering the same id twice is a conflict, not a silent overwrite.
        let (status, _) = post(Arc::clone(&state), "/sources", &definition).await;
        assert_eq!(status, StatusCode::CONFLICT, "{kind}");
    }

    let registered = get_json(Arc::clone(&state), "/sources").await;
    let ids: Vec<&str> = registered
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.iter().any(|id| id.ends_with("-rejected")),
        "a source that failed to connect was registered anyway: {ids:?}"
    );
}

/// A pool built by good credentials must never vouch for bad ones. The
/// PostgreSQL connector caches pools per `(source id, database)`, so without a
/// credential fingerprint in the key a wrong password would come back "ok".
#[tokio::test]
async fn a_cached_pool_does_not_mask_wrong_credentials() {
    let Some(path) = std::env::var_os("ALKYON_SOURCES") else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let raw = std::fs::read_to_string(&path).unwrap();
    let definitions: Vec<Value> = serde_json::from_str(&raw).unwrap();
    let state = AppState::new();

    for definition in definitions {
        let kind = definition["kind"].as_str().unwrap().to_owned();

        // Warm whatever cache the connector keeps.
        let (status, body) = post(Arc::clone(&state), "/connection-test", &definition).await;
        assert_eq!(status, StatusCode::OK, "{kind}: {body}");

        // Same id and database, wrong password. This must still be refused.
        let mut wrong = definition.clone();
        wrong["auth"]["password"] = json!("definitely-not-the-password");
        let (status, body) = post(Arc::clone(&state), "/connection-test", &wrong).await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "{kind}: a wrong password was accepted after a good one: {body}"
        );
    }
}

/// Read messages until `end`, `error` or `cancelled`, returning them in order.
async fn drain<S>(socket: &mut S) -> Vec<Value>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut messages = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(text.as_str()).expect("JSON message");
        let last = matches!(
            value["type"].as_str(),
            Some("end") | Some("error") | Some("cancelled")
        );
        messages.push(value);
        if last {
            break;
        }
    }
    messages
}

#[tokio::test]
async fn websocket_streams_a_result_set() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let sources = state.summaries().await;
    let addr = serve(Arc::clone(&state)).await;

    for source in sources {
        if !relational(source.kind) {
            continue;
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/query"))
            .await
            .expect("websocket upgrade");

        socket
            .send(Message::text(
                json!({
                    "source_id": source.id,
                    "sql": "SELECT order_id, unit_price FROM sales.order_line ORDER BY order_id, line_no",
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let messages = drain(&mut socket).await;
        let kinds: Vec<&str> = messages
            .iter()
            .map(|m| m["type"].as_str().unwrap())
            .collect();
        let id = &source.id;

        assert_eq!(kinds.first(), Some(&"columns"), "{id}: {kinds:?}");
        assert_eq!(kinds.last(), Some(&"end"), "{id}: {kinds:?}");
        assert!(
            kinds.iter().filter(|k| **k == "rows").count() >= 3,
            "{id}: 1500 rows should arrive in several batches, got {kinds:?}"
        );

        assert_eq!(messages[0]["columns"][0]["name"], "order_id");
        assert_eq!(messages.last().unwrap()["rows"], json!(1500));
    }
}

/// A result comes back a page at a time, and the query stays open between pages.
///
/// The rows must not repeat or go missing across the boundary. That is the whole
/// reason the cursor is held open instead of re-running with an `OFFSET`: this
/// statement has an `ORDER BY`, but plenty do not, and a second run of an
/// unordered query is free to hand back a different order.
#[tokio::test]
async fn websocket_pages_a_result_without_losing_rows() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let sources = state.summaries().await;
    let addr = serve(Arc::clone(&state)).await;

    for source in sources {
        if !relational(source.kind) {
            continue;
        }
        let id = &source.id;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/query"))
            .await
            .unwrap();

        // 1500 rows in the seed, 400 to a page: four pages, the last one short.
        socket
            .send(Message::text(
                json!({
                    "source_id": source.id,
                    "page_size": 400,
                    "sql": "SELECT order_id, line_no FROM sales.order_line ORDER BY order_id, line_no",
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let mut seen: Vec<Value> = Vec::new();
        let mut pages = 0usize;
        loop {
            let messages = drain(&mut socket).await;
            let end = messages.last().expect("a page ends");
            assert_eq!(end["type"], "end", "{id}: {messages:?}");

            pages += 1;
            assert_eq!(end["page"].as_u64(), Some(pages as u64), "{id}");
            for message in &messages {
                if message["type"] == "rows" {
                    seen.extend(message["rows"].as_array().unwrap().iter().cloned());
                }
            }
            // Only the last page may be short.
            let expected = if end["more"] == json!(true) { 400 } else { 300 };
            assert_eq!(end["rows"].as_u64(), Some(expected), "{id}: page {pages}");

            if end["more"] != json!(true) {
                break;
            }
            socket
                .send(Message::text(json!({ "type": "more" }).to_string()))
                .await
                .unwrap();
        }

        assert_eq!(pages, 4, "{id}: 1500 rows at 400 a page");
        assert_eq!(seen.len(), 1500, "{id}: every row arrives exactly once");

        // Ordered by (order_id, line_no), so the sequence must be strictly
        // increasing across page boundaries — no repeats, no gaps.
        let keys: Vec<(i64, i64)> = seen
            .iter()
            .map(|row| (row[0].as_i64().unwrap(), row[1].as_i64().unwrap()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(keys, sorted, "{id}: pages overlapped or skipped");

        // Columns are announced once, on the first page only.
        socket.close(None).await.ok();
    }
}

#[tokio::test]
async fn websocket_reports_a_bad_statement() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let sources = state.summaries().await;
    let addr = serve(Arc::clone(&state)).await;

    for source in sources {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/query"))
            .await
            .unwrap();
        socket
            .send(Message::text(
                json!({ "source_id": source.id, "sql": "SELECT * FROM sales.no_such_table" })
                    .to_string(),
            ))
            .await
            .unwrap();

        let messages = drain(&mut socket).await;
        assert_eq!(messages.len(), 1, "{}: {messages:?}", source.id);
        assert_eq!(messages[0]["type"], "error");
    }
}

#[tokio::test]
async fn websocket_cancels_a_running_query() {
    let Some(state) = seeded().await else {
        eprintln!("skipped: ALKYON_SOURCES is not set");
        return;
    };
    let sources = state.summaries().await;
    let addr = serve(Arc::clone(&state)).await;

    for source in sources {
        // A statement that blocks long enough that the cancel cannot lose the race.
        let sql = match source.kind {
            alkyon::model::SourceKind::Postgres => "SELECT pg_sleep(30)",
            alkyon::model::SourceKind::MsSql => "WAITFOR DELAY '00:00:30'",
            alkyon::model::SourceKind::MySql => "SELECT SLEEP(30)",
            // DuckDB has no sleep, and anything slow enough to race here would
            // keep burning a thread for the rest of the suite: the blocking
            // worker only learns of the cancel when it next tries to send.
            // DuckDB answers for these, and it has no sleep — anything slow
            // enough to race here would keep burning a thread for the rest of
            // the suite.
            alkyon::model::SourceKind::Folder
            | alkyon::model::SourceKind::File
            | alkyon::model::SourceKind::Adls
            | alkyon::model::SourceKind::Mongo => continue,
        };

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/query"))
            .await
            .unwrap();
        socket
            .send(Message::text(
                json!({ "source_id": source.id, "sql": sql }).to_string(),
            ))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;
        socket
            .send(Message::text(json!({ "type": "cancel" }).to_string()))
            .await
            .unwrap();

        let messages = tokio::time::timeout(Duration::from_secs(5), drain(&mut socket))
            .await
            .unwrap_or_else(|_| panic!("{}: cancel was not acknowledged", source.id));
        assert_eq!(
            messages.last().unwrap()["type"],
            "cancelled",
            "{}",
            source.id
        );
    }
}
