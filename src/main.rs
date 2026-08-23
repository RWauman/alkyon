use std::error::Error;
// `axum::serve(..).with_graceful_shutdown(..)` is an `IntoFuture`, not a `Future`.
use std::future::IntoFuture;
use std::net::SocketAddr;

use alkyon::model::SourceConfig;
use alkyon::state::{self, AppState};
use alkyon::vault::Vault;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "desktop")]
mod desktop;

const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// Not `#[tokio::main]` any more: the window's event loop has to own the main
/// thread, so the runtime is built by hand and the server runs on it in the
/// background. Headless, the shape is what it always was.
fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ALKYON_LOG")
                .unwrap_or_else(|_| EnvFilter::new("alkyon=info,tower_http=warn")),
        )
        .init();

    let asked = std::env::var("ALKYON_BIND");
    let wanted: SocketAddr = asked
        .as_deref()
        .unwrap_or(DEFAULT_BIND)
        .parse()
        .map_err(|e| format!("ALKYON_BIND is not an address: {e}"))?;
    // An address of one's own is a deliberate choice, and the two conveniences it
    // turns off are both about not surprising whoever made it: the port is never
    // silently changed, and a server already sitting there is never adopted.
    let ours_to_choose = asked.is_err();

    let windowed = windowed();
    tracing::debug!(windowed, "starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Double-clicking twice is an ordinary thing to do to a desktop application,
    // and two servers over one config directory would race each other writing
    // `sources.json`. So a second launch is a second window onto the workbench
    // that is already running rather than a second workbench.
    #[cfg(feature = "desktop")]
    if windowed && ours_to_choose && runtime.block_on(already_running(wanted)) {
        tracing::info!("alkyon is already running on {wanted} — opening a window onto it");
        return desktop::run(wanted);
    }

    let listener = runtime.block_on(listen(wanted, windowed && ours_to_choose))?;
    let address = listener.local_addr()?;

    // The state is built from the address actually bound rather than the one
    // asked for: it is what decides whether the terminal is exposed, and after a
    // fallback the two are not the same port.
    let state = runtime.block_on(prepare(address))?;
    tracing::info!("alkyon listening on http://{address}");

    let router = alkyon::api::router(state);

    #[cfg(feature = "desktop")]
    if windowed {
        // The server becomes a task from here: the window is what the process
        // waits on, and closing it drops the runtime, which stops the server.
        // Nothing is lost on the way out — every registry is written when it
        // changes, not at exit.
        runtime.spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "the server stopped");
            }
        });
        return desktop::run(address);
    }

    runtime.block_on(
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("shutting down");
            })
            .into_future(),
    )?;
    Ok(())
}

/// Whether to open a window.
///
/// `--headless`, or `ALKYON_HEADLESS`, is how a desktop build is run as the plain
/// server it has always been — under Docker, on a machine with no display, or
/// when the point is the HTTP API rather than the editor. Without the feature
/// there is no window to open at all.
fn windowed() -> bool {
    if !cfg!(feature = "desktop") {
        return false;
    }
    let refused = std::env::args().any(|argument| argument == "--headless")
        || std::env::var_os("ALKYON_HEADLESS").is_some();
    !refused
}

/// Bind `address`, falling back to a port the OS picks when it is taken.
///
/// Only for the window: a headless server whose port moved would be a server
/// nothing can find, whereas a window is told where to look.
async fn listen(address: SocketAddr, may_fall_back: bool) -> std::io::Result<TcpListener> {
    match TcpListener::bind(address).await {
        Ok(listener) => Ok(listener),
        Err(e) if may_fall_back && e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!("{address} is taken — listening on a port the OS picks instead");
            TcpListener::bind(SocketAddr::new(address.ip(), 0)).await
        }
        Err(e) => Err(e),
    }
}

/// The vault, the registries, and anything `ALKYON_SOURCES` was pointed at.
async fn prepare(address: SocketAddr) -> Result<std::sync::Arc<AppState>, Box<dyn Error>> {
    let vault = Vault::from_env();
    let config_dir = state::config_dir().ok_or("no config directory: set ALKYON_CONFIG_DIR")?;
    tracing::info!(vault = vault.describe(), config = %config_dir.display(), "starting");

    let state = AppState::load(vault, &config_dir, state::terminal_allowed(&address))?;
    if !state.terminal_enabled {
        tracing::warn!("terminal disabled: {address} is not loopback");
    }

    // A file of sources with credentials is an import, not a config file: the
    // secrets move into the vault and the file can then be deleted.
    if let Some(path) = std::env::var_os("ALKYON_SOURCES") {
        let raw = tokio::fs::read_to_string(&path).await?;
        let configs: Vec<SourceConfig> = serde_json::from_str(&raw)?;
        state.import(configs).await?;
    }
    Ok(state)
}

/// Whether something that *is alkyon* is already serving `address`.
///
/// `/health` names itself — the vault and terminal fields are ours — so an
/// unrelated service holding the port is not mistaken for a workbench and
/// adopted. A short timeout, because this is on the path to a window opening.
#[cfg(feature = "desktop")]
async fn already_running(address: SocketAddr) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(700))
        .build()
    else {
        return false;
    };
    let Ok(response) = client
        .get(format!("http://{address}/health"))
        .send()
        .await
    else {
        return false;
    };
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return false;
    };
    body.get("status").and_then(serde_json::Value::as_str) == Some("ok")
        && body.get("vault").is_some()
        && body.get("terminal").is_some()
}
