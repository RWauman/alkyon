//! The window, when alkyon is a desktop application rather than a page.
//!
//! **It changes nothing about the architecture.** Tauri opens the platform's own
//! webview pointed at `http://127.0.0.1:<port>` — the same UI, served by the same
//! axum router, talking to the same HTTP and WebSocket API. There is no second
//! front end and no IPC bridge: nothing in `src/ui/` knows whether it is in this
//! window or in a browser tab, which is what the README meant by there being no
//! separate codebase for the desktop build.
//!
//! That is also why `tauri.conf.json` gives a **URL** as its `frontendDist`
//! rather than a directory: Tauri then bundles no assets of its own, and the ones
//! `rust-embed` already carries are not embedded a second time.
//!
//! **No browser is bundled.** The webview is the one the platform already
//! ships — WebView2 on Windows, WebKitGTK on Linux, WKWebView on macOS — so the
//! binary stays the size DuckDB made it instead of gaining a hundred megabytes of
//! Chromium. The cost is one system dependency on Linux, beside the `libdbus` the
//! keychain already needs.
//!
//! The window owns the **main thread**: every desktop platform requires its event
//! loop to run there, which is why `main` builds the runtime by hand and ends
//! here.

use std::error::Error;
use std::net::SocketAddr;

/// Big enough for the three panes alkyon opens with — explorer, editor and a
/// result grid — without assuming a large screen.
const SIZE: (f64, f64) = (1440.0, 900.0);

/// Below this the grid stops being readable and the explorer eats the editor.
const SMALLEST: (f64, f64) = (900.0, 560.0);

/// Open the window on the server at `address` and run until it is closed.
pub fn run(address: SocketAddr) -> Result<(), Box<dyn Error>> {
    let url = format!("http://{address}");
    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse()?),
            )
            .title("Alkyon")
            .inner_size(SIZE.0, SIZE.1)
            .min_inner_size(SMALLEST.0, SMALLEST.1)
            .build()?;
            tracing::info!(url = %url, "window open");
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
