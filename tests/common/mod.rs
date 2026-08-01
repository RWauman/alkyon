use std::sync::Arc;

use alkyon::model::SourceConfig;
use alkyon::state::AppState;

/// Load the sources named by `ALKYON_SOURCES`, or `None` when it is unset — which
/// is how the tests that need a live server skip themselves.
pub async fn seeded() -> Option<Arc<AppState>> {
    let path = std::env::var_os("ALKYON_SOURCES")?;
    let raw = std::fs::read_to_string(&path).expect("ALKYON_SOURCES must be readable");
    let configs: Vec<SourceConfig> =
        serde_json::from_str(&raw).expect("ALKYON_SOURCES must be a JSON array of sources");
    let state = AppState::new();
    state
        .import(configs)
        .await
        .expect("import into memory vault");
    Some(state)
}
