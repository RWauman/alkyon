use std::sync::Arc;

use alkyon::model::{SourceConfig, SourceKind};
use alkyon::state::AppState;

/// Whether this source carries the shared **relational** seed — `sales.customer`
/// with a primary key, `sales.order_line` with 1500 rows, `sales.order_value` as a
/// view.
///
/// MongoDB does not, on purpose: its seed is document-shaped, so its collections
/// live in `alkyon_demo` rather than a `sales` schema, nothing is a primary key,
/// and a field may be missing from a document. A test that walks every source and
/// asserts that seed is asserting the wrong thing about it — `tests/mongo.rs`
/// asserts the right one. The traits and routes are exercised for every kind
/// either way.
// Each test binary compiles this module for itself, so a helper only some of them
// need is dead code in the rest.
#[allow(dead_code)]
pub fn relational(kind: SourceKind) -> bool {
    kind != SourceKind::Mongo
}

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
