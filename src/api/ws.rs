use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::model::{ColumnMeta, RowBatch};
use crate::state::AppState;

pub async fn query(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| session(socket, state))
}

#[derive(Deserialize)]
struct QueryRequest {
    source_id: String,
    /// Which database on the source to run against. Defaults to the source's own.
    #[serde(default)]
    database: Option<String>,
    sql: String,
    /// Stop after this many rows. `0` means no cap, which is the client asking
    /// for it explicitly. Absent means [`max_rows`]'s default.
    #[serde(default)]
    max_rows: Option<usize>,
}

/// How many rows a query may send back before it is stopped.
///
/// Not a nicety. A single 2.5M-row parquet is 382 MB of JSON on the wire and
/// roughly 800 MB of browser heap once parsed; a `select *` typed by reflex used
/// to take the tab with it. Stopping early also *cancels* the query, because
/// dropping the stream is what cancellation already means here.
///
/// The client sends its own value — the picker in the header — so this is only
/// the floor for anything that does not.
fn max_rows(request: &QueryRequest) -> Option<usize> {
    let configured = request.max_rows.unwrap_or_else(|| {
        std::env::var("ALKYON_MAX_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(50_000)
    });
    (configured > 0).then_some(configured)
}

#[derive(Deserialize)]
struct Control {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Columns {
        columns: Arc<Vec<ColumnMeta>>,
    },
    Rows {
        rows: Vec<Vec<Value>>,
    },
    Affected {
        rows_affected: u64,
    },
    End {
        rows: usize,
        elapsed_ms: u64,
        /// The cap was hit and the query was stopped — there were more rows.
        truncated: bool,
    },
    Cancelled,
    Error {
        message: String,
    },
}

type Sender = SplitSink<WebSocket, Message>;
type Receiver = SplitStream<WebSocket>;

/// One socket handles one query at a time, sequentially. Sending
/// `{"type":"cancel"}` while a query is streaming drops the database stream,
/// which is what actually cancels it.
async fn session(socket: WebSocket, state: Arc<AppState>) {
    let (mut tx, mut rx) = socket.split();

    while let Some(Ok(message)) = rx.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };

        let request: QueryRequest = match serde_json::from_str(text.as_str()) {
            Ok(request) => request,
            Err(e) => {
                let _ = send(
                    &mut tx,
                    ServerMessage::Error {
                        message: format!("bad request: {e}"),
                    },
                )
                .await;
                continue;
            }
        };

        let outcome = tokio::select! {
            result = run(&state, &request, &mut tx) => Some(result),
            () = wait_for_cancel(&mut rx) => None,
        };

        let closing = match outcome {
            Some(Ok(())) => continue,
            Some(Err(e)) => {
                send(
                    &mut tx,
                    ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await
            }
            None => send(&mut tx, ServerMessage::Cancelled).await,
        };
        if closing.is_err() {
            break;
        }
    }
}

async fn run(state: &AppState, request: &QueryRequest, tx: &mut Sender) -> Result<()> {
    // A `-- @duckdb` preamble routes the buffer to the federator instead of to a
    // single source. Checked here rather than in the client so that the buffer is
    // the only source of truth about which engine runs it.
    if crate::federation::program::is_federated(&request.sql) {
        return run_federated(state, request, tx).await;
    }

    let started = Instant::now();
    let connection = state
        .open(&request.source_id, request.database.as_deref())
        .await?;

    let (rows, truncated) = pump(connection.execute(&request.sql), max_rows(request), tx).await?;

    send(
        tx,
        ServerMessage::End {
            rows,
            elapsed_ms: started.elapsed().as_millis() as u64,
            truncated,
        },
    )
    .await
}

/// Forward a result stream to the socket, stopping at `cap` rows.
///
/// Returns the row count and whether there were more. Breaking out of the loop
/// drops the stream, and dropping the stream is what cancels the query — so a
/// capped `select *` does not go on reading a 600 MB parquet in the background.
async fn pump(
    mut batches: futures::stream::BoxStream<'_, Result<RowBatch>>,
    cap: Option<usize>,
    tx: &mut Sender,
) -> Result<(usize, bool)> {
    let mut rows = 0usize;
    let mut truncated = false;

    while let Some(batch) = batches.next().await {
        match batch? {
            RowBatch::Columns(columns) => send(tx, ServerMessage::Columns { columns }).await?,
            RowBatch::Affected(rows_affected) => {
                send(tx, ServerMessage::Affected { rows_affected }).await?
            }
            RowBatch::Rows(mut batch) => {
                // Strictly greater, so a result that lands exactly on the cap is
                // reported whole rather than as "there is more" when there is not.
                if cap.is_some_and(|cap| rows + batch.len() > cap) {
                    batch.truncate(cap.unwrap_or(0) - rows);
                    truncated = true;
                }
                rows += batch.len();
                if !batch.is_empty() {
                    send(tx, ServerMessage::Rows { rows: batch }).await?;
                }
                if truncated {
                    break;
                }
            }
        }
    }
    Ok((rows, truncated))
}

/// The federated path. Same messages out, so the client does not care which
/// engine answered.
async fn run_federated(state: &AppState, request: &QueryRequest, tx: &mut Sender) -> Result<()> {
    let started = Instant::now();
    let program = crate::federation::program::parse(&request.sql)?;
    let imports = program.imports.len();

    let batches = crate::federation::execute(state, program, crate::federation::Limits::from_env());
    let (rows, truncated) = pump(batches, max_rows(request), tx).await?;

    tracing::info!(imports, rows, truncated, "federated query finished");
    send(
        tx,
        ServerMessage::End {
            rows,
            elapsed_ms: started.elapsed().as_millis() as u64,
            truncated,
        },
    )
    .await
}

/// Resolves when the client asks to cancel, or when the socket goes away.
async fn wait_for_cancel(rx: &mut Receiver) {
    while let Some(Ok(message)) = rx.next().await {
        match message {
            Message::Text(text) => {
                if matches!(
                    serde_json::from_str::<Control>(text.as_str()),
                    Ok(Control { kind }) if kind == "cancel"
                ) {
                    return;
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

async fn send(tx: &mut Sender, message: ServerMessage) -> Result<()> {
    let payload = serde_json::to_string(&message)?;
    tx.send(Message::Text(payload.into())).await?;
    Ok(())
}
