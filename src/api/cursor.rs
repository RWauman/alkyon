//! A query held open across requests, handing out one page at a time.
//!
//! The alternative — re-running the query with `OFFSET` for each page — is both
//! slower and *wrong*: a statement with no `ORDER BY` may hand back a different
//! order on the second run, so page two could repeat or skip rows from page one.
//! Reading further down a stream that was never restarted cannot do that.
//!
//! The query lives in its own task because a result stream borrows the
//! connection that produced it. Both are locals of that task, so the pair never
//! has to be stored or returned, and the borrow stays ordinary.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::model::RowBatch;
use crate::state::AppState;

/// What the reader task sends back while filling a page.
pub enum Chunk {
    Batch(RowBatch),
    /// The page is complete. `more` is false once the result is exhausted.
    PageEnd {
        more: bool,
    },
    Failed(Error),
}

/// A running query, positioned wherever the last page left off.
pub struct Cursor {
    /// How many rows the next page should hold.
    demand: mpsc::Sender<usize>,
    chunks: mpsc::Receiver<Chunk>,
    /// True once the result set has run out; the task is gone by then.
    finished: bool,
}

impl Cursor {
    /// Start `sql` and leave it open. Nothing is read until [`Cursor::page`].
    pub fn open(
        state: Arc<AppState>,
        source: String,
        database: Option<String>,
        sql: String,
    ) -> Self {
        let (demand, mut wanted) = mpsc::channel::<usize>(1);
        // One chunk in flight: the reader must not run ahead of the socket, or
        // the memory this whole design exists to bound comes back.
        let (sender, chunks) = mpsc::channel::<Chunk>(1);

        tokio::spawn(async move {
            // A `-- @duckdb` preamble routes the buffer to the federator rather
            // than to one source. Decided here, from the buffer, so the text
            // stays the only thing that says which engine runs it.
            if crate::federation::program::is_federated(&sql) {
                let program = match crate::federation::program::parse(&sql) {
                    Ok(program) => program,
                    Err(e) => {
                        let _ = sender.send(Chunk::Failed(e)).await;
                        return;
                    }
                };
                let mut stream = crate::federation::execute(
                    &state,
                    program,
                    crate::federation::Limits::from_env(),
                );
                serve(&mut stream, &mut wanted, &sender).await;
            } else {
                let connection = match state.open(&source, database.as_deref()).await {
                    Ok(connection) => connection,
                    Err(e) => {
                        let _ = sender.send(Chunk::Failed(e)).await;
                        return;
                    }
                };
                // Borrowing `connection` here is fine: both live in this task's
                // frame and neither is moved again.
                let mut stream = connection.execute(&sql);
                serve(&mut stream, &mut wanted, &sender).await;
            }
        });

        Self {
            demand,
            chunks,
            finished: false,
        }
    }

    /// Ask for the next `rows`, then read the chunks off [`Cursor::chunks`].
    ///
    /// Returns false once the cursor is spent, so a stray "next page" after the
    /// last one is a no-op rather than an error.
    pub async fn request(&mut self, rows: usize) -> bool {
        if self.finished || self.demand.send(rows).await.is_err() {
            self.finished = true;
            return false;
        }
        true
    }

    pub async fn next_chunk(&mut self) -> Option<Chunk> {
        self.chunks.recv().await
    }

    /// Called once the caller has seen `PageEnd { more: false }`.
    pub fn exhaust(&mut self) {
        self.finished = true;
    }
}

/// Answer demands for pages until the socket stops asking or the rows run out.
async fn serve(
    stream: &mut BoxStream<'_, Result<RowBatch>>,
    wanted: &mut mpsc::Receiver<usize>,
    sender: &mpsc::Sender<Chunk>,
) {
    // Rows pulled for a page that turned out to be full, kept for the next one.
    let mut carried: Vec<Vec<Value>> = Vec::new();

    while let Some(want) = wanted.recv().await {
        match fill(stream, &mut carried, want, sender).await {
            Ok(more) => {
                if sender.send(Chunk::PageEnd { more }).await.is_err() || !more {
                    return;
                }
            }
            Err(e) => {
                let _ = sender.send(Chunk::Failed(e)).await;
                return;
            }
        }
    }
}

/// Send rows until `want` is reached or the stream ends. `Ok(true)` means there
/// is more to come.
async fn fill(
    stream: &mut BoxStream<'_, Result<RowBatch>>,
    carried: &mut Vec<Vec<Value>>,
    want: usize,
    sender: &mpsc::Sender<Chunk>,
) -> Result<bool> {
    let mut sent = 0usize;

    // Whatever overflowed the previous page opens this one.
    if !carried.is_empty() {
        let mut batch = std::mem::take(carried);
        if batch.len() > want {
            *carried = batch.split_off(want);
        }
        sent += batch.len();
        if sender
            .send(Chunk::Batch(RowBatch::Rows(batch)))
            .await
            .is_err()
        {
            return Ok(false);
        }
    }

    while sent < want {
        let Some(batch) = stream.next().await else {
            return Ok(false);
        };
        match batch? {
            RowBatch::Rows(mut rows) => {
                // A batch that overflows the page is split, and the tail waits.
                if sent + rows.len() > want {
                    *carried = rows.split_off(want - sent);
                }
                sent += rows.len();
                if !rows.is_empty()
                    && sender
                        .send(Chunk::Batch(RowBatch::Rows(rows)))
                        .await
                        .is_err()
                {
                    return Ok(false);
                }
            }
            other => {
                if sender.send(Chunk::Batch(other)).await.is_err() {
                    return Ok(false);
                }
            }
        }
    }

    // The page is full. Whether anything follows is only knowable by looking, so
    // look once and carry what we find — that is also what makes "there is more"
    // an answer rather than a guess.
    if !carried.is_empty() {
        return Ok(true);
    }
    loop {
        match stream.next().await {
            None => return Ok(false),
            Some(batch) => match batch? {
                RowBatch::Rows(rows) => {
                    if rows.is_empty() {
                        continue;
                    }
                    *carried = rows;
                    return Ok(true);
                }
                // A statement that reports only a row count still counts as more
                // to come: the client has yet to be told about it.
                other => {
                    if sender.send(Chunk::Batch(other)).await.is_err() {
                        return Ok(false);
                    }
                }
            },
        }
    }
}
