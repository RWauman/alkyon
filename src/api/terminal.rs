use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::error::Error;
use crate::state::AppState;

/// Read from the shell in chunks this size. Big enough that `cat` of a large file
/// does not turn into thousands of WebSocket frames.
const READ_CHUNK: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

#[derive(Deserialize)]
pub struct TerminalQuery {
    /// A shell name from `GET /shells`. Omitted means the default.
    #[serde(default)]
    shell: Option<String>,
}

pub async fn terminal(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<TerminalQuery>,
) -> Response {
    if !state.terminal_enabled {
        // Refusing here rather than not mounting the route at all, so the reason
        // reaches whoever is looking at the UI.
        return (
            StatusCode::FORBIDDEN,
            "the terminal is disabled on a non-loopback bind; set ALKYON_TERMINAL=always to override",
        )
            .into_response();
    }
    let cwd = state.workspace().await;
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = session(socket, params.shell.as_deref(), cwd).await {
            tracing::error!(error = %e, "terminal session failed");
        }
    })
}

/// Shells worth offering, best first. Only those actually present are returned.
#[cfg(windows)]
const CANDIDATES: &[&str] = &[
    "pwsh.exe",
    "powershell.exe",
    "cmd.exe",
    "bash.exe",
    "wsl.exe",
];
#[cfg(not(windows))]
const CANDIDATES: &[&str] = &["zsh", "bash", "fish", "sh"];

fn on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The shells this machine actually has. `ALKYON_SHELL`, if set, always comes
/// first and is the default.
///
/// The picker sends back a name from this list and nothing else — an arbitrary
/// command from the client would turn a fixed shell into arbitrary execution,
/// which matters on a `ALKYON_TERMINAL=always` bind.
pub fn available() -> Vec<(String, String)> {
    let mut shells = Vec::new();
    if let Some(forced) = std::env::var_os("ALKYON_SHELL") {
        let forced = forced.to_string_lossy().into_owned();
        shells.push((forced.clone(), forced));
    }
    for name in CANDIDATES {
        if let Some(path) = on_path(name) {
            let label = name.trim_end_matches(".exe").to_owned();
            if !shells.iter().any(|(l, _)| l == &label) {
                shells.push((label, path.to_string_lossy().into_owned()));
            }
        }
    }
    shells
}

pub async fn shells(State(state): State<Arc<AppState>>) -> Json<Value> {
    let found = available();
    Json(json!({
        "enabled": state.terminal_enabled,
        "default": found.first().map(|(label, _)| label.clone()),
        "shells": found
            .iter()
            .map(|(label, path)| json!({ "name": label, "path": path }))
            .collect::<Vec<_>>(),
    }))
}

/// Build the command for `requested`, falling back to the default when it is
/// absent or not on the allowlist. `cwd` is the open folder, if any.
fn shell(requested: Option<&str>, cwd: Option<PathBuf>) -> Result<CommandBuilder, Error> {
    let found = available();
    let (_, program) = match requested {
        Some(name) => found
            .iter()
            .find(|(label, _)| label == name)
            .ok_or_else(|| Error::Terminal(format!("unknown shell `{name}`")))?,
        None => found
            .first()
            .ok_or_else(|| Error::Terminal("no shell found on PATH".into()))?,
    };

    let mut command = CommandBuilder::new(program);
    command.env("TERM", "xterm-256color");

    // The open folder if there is one — that is the whole point of opening it.
    // Otherwise home: an installed binary can be launched from anywhere,
    // including a directory the user has no business being in.
    if let Some(cwd) = cwd {
        command.cwd(cwd);
    } else if let Some(dirs) = directories::UserDirs::new() {
        command.cwd(dirs.home_dir());
    }
    Ok(command)
}

async fn session(
    socket: WebSocket,
    requested: Option<&str>,
    cwd: Option<PathBuf>,
) -> crate::error::Result<()> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::Terminal(e.to_string()))?;

    let mut child = pty
        .slave
        .spawn_command(shell(requested, cwd)?)
        .map_err(|e| Error::Terminal(e.to_string()))?;
    // Dropping our end of the slave means the reader sees EOF when the shell
    // exits, instead of blocking forever.
    drop(pty.slave);

    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| Error::Terminal(e.to_string()))?;
    let mut writer = pty
        .master
        .take_writer()
        .map_err(|e| Error::Terminal(e.to_string()))?;

    // The PTY handles are blocking, and both loops run for the life of the
    // session, so they get real threads rather than blocking-pool slots.
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output_tx.send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Some(chunk) = input_rx.blocking_recv() {
            if writer.write_all(&chunk).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    let (mut sink, mut stream) = socket.split();
    let mut pump = tokio::spawn(async move {
        while let Some(chunk) = output_rx.recv().await {
            if sink.send(Message::Binary(chunk.into())).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    loop {
        tokio::select! {
            // The shell exited, or the browser stopped listening.
            _ = &mut pump => break,
            message = stream.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if input_tx.send(bytes.to_vec()).is_err() {
                        break;
                    }
                }
                // Text frames are control messages, never keystrokes — that is
                // what keeps a resize from being typed into the shell.
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<Control>(text.as_str()) {
                        Ok(Control::Resize { cols, rows }) => {
                            let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
                            if let Err(e) = pty.master.resize(size) {
                                tracing::warn!(error = %e, "resize failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "unrecognised terminal control"),
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    pump.abort();
    Ok(())
}
