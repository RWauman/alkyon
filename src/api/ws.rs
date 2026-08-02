use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cursor::{Chunk, Cursor};
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
    /// Rows per page. Absent falls back to [`page_size`]'s default.
    #[serde(default)]
    page_size: Option<usize>,
}

/// How many rows one page holds.
///
/// Every result is paged, and this is the page. Nothing here is a preference:
/// the whole of one taxi folder is 39.6M rows and **6.16 GB** of JSON — the
/// server streams that in 106 seconds with flat memory, and a browser tab dies
/// around 14M rows trying to hold it. A page is what keeps the browser's share
/// of a result the same size whatever the result is.
fn page_size(request: &QueryRequest) -> usize {
    request
        .page_size
        .filter(|size| *size > 0)
        .unwrap_or_else(|| {
            std::env::var("ALKYON_PAGE_ROWS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|size| *size > 0)
                .unwrap_or(50_000)
        })
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
    /// End of a *page*, not of the result.
    End {
        /// Rows in this page.
        rows: usize,
        /// Which page this was, counting from one.
        page: usize,
        elapsed_ms: u64,
        /// Whether asking again would bring anything back.
        more: bool,
    },
    Cancelled,
    Error {
        message: String,
    },
}

type Sender = SplitSink<WebSocket, Message>;
type Receiver = SplitStream<WebSocket>;

/// One socket runs one query at a time and keeps it open between pages.
///
/// A query message starts a fresh cursor; `{"type":"more"}` reads the next page
/// from the one already running; `{"type":"cancel"}` drops it, which is what
/// actually stops the engine.
async fn session(socket: WebSocket, state: Arc<AppState>) {
    let (mut tx, mut rx) = socket.split();
    let mut cursor: Option<Cursor> = None;
    let mut page = 0usize;
    let mut rows_per_page = 0usize;

    while let Some(Ok(message)) = rx.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };

        // `more` and `cancel` are the two controls; anything else is a query.
        if let Ok(Control { kind }) = serde_json::from_str::<Control>(text.as_str()) {
            match kind.as_str() {
                "cancel" => {
                    // Dropping the cursor drops the stream, and dropping the
                    // stream is what cancellation has always meant here.
                    cursor = None;
                    if send(&mut tx, ServerMessage::Cancelled).await.is_err() {
                        break;
                    }
                    continue;
                }
                "more" => {
                    let Some(open) = cursor.as_mut() else {
                        let _ = send(
                            &mut tx,
                            ServerMessage::Error {
                                message: "no query is open — run one first".into(),
                            },
                        )
                        .await;
                        continue;
                    };
                    page += 1;
                    match deliver(open, rows_per_page, page, &mut tx, &mut rx).await {
                        Ok(true) => {}
                        // Cancelled: the page number never happened.
                        Ok(false) => {
                            page -= 1;
                            cursor = None;
                        }
                        Err(_) => break,
                    }
                    continue;
                }
                _ => {}
            }
        }

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

        rows_per_page = page_size(&request);
        page = 1;
        let mut open = Cursor::open(
            Arc::clone(&state),
            request.source_id.clone(),
            request.database.clone(),
            request.sql.clone(),
        );
        match deliver(&mut open, rows_per_page, page, &mut tx, &mut rx).await {
            Ok(true) => cursor = Some(open),
            Ok(false) => cursor = None,
            Err(_) => break,
        }
    }
}

/// Read one page off `cursor` and forward it. `Err` only when the socket died.
///
/// `rx` is watched throughout: a page can take a minute, and a cancel that only
/// arrives once the page is finished is not a cancel. Returns `Ok(false)` when
/// the client gave up on this one.
async fn deliver(
    cursor: &mut Cursor,
    rows_per_page: usize,
    page: usize,
    tx: &mut Sender,
    rx: &mut Receiver,
) -> Result<bool> {
    let started = Instant::now();

    if !cursor.request(rows_per_page).await {
        send(
            tx,
            ServerMessage::End {
                rows: 0,
                page,
                elapsed_ms: 0,
                more: false,
            },
        )
        .await?;
        return Ok(true);
    }

    let mut rows = 0usize;
    loop {
        let chunk = tokio::select! {
            chunk = cursor.next_chunk() => chunk,
            () = wait_for_cancel(rx) => {
                cursor.exhaust();
                send(tx, ServerMessage::Cancelled).await?;
                return Ok(false);
            }
        };
        let Some(chunk) = chunk else { break };

        match chunk {
            Chunk::Batch(RowBatch::Columns(columns)) => {
                send(tx, ServerMessage::Columns { columns }).await?
            }
            Chunk::Batch(RowBatch::Rows(batch)) => {
                rows += batch.len();
                send(tx, ServerMessage::Rows { rows: batch }).await?;
            }
            Chunk::Batch(RowBatch::Affected(rows_affected)) => {
                send(tx, ServerMessage::Affected { rows_affected }).await?
            }
            Chunk::Failed(e) => {
                cursor.exhaust();
                send(
                    tx,
                    ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                return Ok(true);
            }
            Chunk::PageEnd { more } => {
                if !more {
                    cursor.exhaust();
                }
                send(
                    tx,
                    ServerMessage::End {
                        rows,
                        page,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        more,
                    },
                )
                .await?;
                return Ok(true);
            }
        }
    }

    // The reader task went away without a verdict.
    cursor.exhaust();
    send(
        tx,
        ServerMessage::End {
            rows,
            page,
            elapsed_ms: started.elapsed().as_millis() as u64,
            more: false,
        },
    )
    .await?;
    Ok(true)
}

/// Resolves when the client asks to cancel, or when the socket goes away.
///
/// Anything else that arrives mid-page is dropped: the socket runs one query at
/// a time, and a second one sent before the first finished has no answer to go
/// back to.
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
