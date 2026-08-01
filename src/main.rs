use std::error::Error;
use std::net::SocketAddr;

use alkyon::model::SourceConfig;
use alkyon::state::{self, AppState};
use alkyon::vault::Vault;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ALKYON_LOG")
                .unwrap_or_else(|_| EnvFilter::new("alkyon=info,tower_http=warn")),
        )
        .init();

    let bind: SocketAddr = std::env::var("ALKYON_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_owned())
        .parse()?;

    let vault = Vault::from_env();
    let config_dir = state::config_dir().ok_or("no config directory: set ALKYON_CONFIG_DIR")?;
    tracing::info!(vault = vault.describe(), config = %config_dir.display(), "starting");

    let state = AppState::load(vault, &config_dir, state::terminal_allowed(&bind))?;
    if !state.terminal_enabled {
        tracing::warn!("terminal disabled: {bind} is not loopback");
    }

    // A file of sources with credentials is an import, not a config file: the
    // secrets move into the vault and the file can then be deleted.
    if let Some(path) = std::env::var_os("ALKYON_SOURCES") {
        let raw = tokio::fs::read_to_string(&path).await?;
        let configs: Vec<SourceConfig> = serde_json::from_str(&raw)?;
        state.import(configs).await?;
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("alkyon listening on http://{}", listener.local_addr()?);

    axum::serve(listener, alkyon::api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;

    Ok(())
}
