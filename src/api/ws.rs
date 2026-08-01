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
}

#[derive(Deserialize)]
struct Control {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Columns { columns: Arc<Vec<ColumnMeta>> },
    Rows { rows: Vec<Vec<Value>> },
    Affected { rows_affected: u64 },
    End { rows: usize, elapsed_ms: u64 },
    Cancelled,
    Error { message: String },
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
        return run_federated(state, &request.sql, tx).await;
    }

    let started = Instant::now();
    let connection = state
        .open(&request.source_id, request.database.as_deref())
        .await?;

    let mut batches = connection.execute(&request.sql);
    let mut rows = 0usize;

    while let Some(batch) = batches.next().await {
        let message = match batch? {
            RowBatch::Columns(columns) => ServerMessage::Columns { columns },
            RowBatch::Rows(batch) => {
                rows += batch.len();
                ServerMessage::Rows { rows: batch }
            }
            RowBatch::Affected(rows_affected) => ServerMessage::Affected { rows_affected },
        };
        send(tx, message).await?;
    }

    send(
        tx,
        ServerMessage::End {
            rows,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
    )
    .await
}

/// The federated path. Same messages out, so the client does not care which
/// engine answered.
async fn run_federated(state: &AppState, sql: &str, tx: &mut Sender) -> Result<()> {
    let started = Instant::now();
    let program = crate::federation::program::parse(sql)?;
    let imports = program.imports.len();

    let mut batches =
        crate::federation::execute(state, program, crate::federation::Limits::from_env());
    let mut rows = 0usize;

    while let Some(batch) = batches.next().await {
        let message = match batch? {
            RowBatch::Columns(columns) => ServerMessage::Columns { columns },
            RowBatch::Rows(batch) => {
                rows += batch.len();
                ServerMessage::Rows { rows: batch }
            }
            RowBatch::Affected(rows_affected) => ServerMessage::Affected { rows_affected },
        };
        send(tx, message).await?;
    }

    tracing::info!(imports, rows, "federated query finished");
    send(
        tx,
        ServerMessage::End {
            rows,
            elapsed_ms: started.elapsed().as_millis() as u64,
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
