//! Only Tauri needs a build step, and only when the desktop shell is being
//! built.
//!
//! Guarded by the feature so that `--no-default-features` — which is how the
//! container image is built — needs neither `tauri.conf.json`, nor the icons, nor
//! the Windows resource compiler. `cargo build` remains the whole build either
//! way; the installers are the one thing that needs the Tauri CLI, the way the
//! result grid is the one thing that needs npm.
fn main() {
    // An attribute rather than a runtime `if` on `CARGO_FEATURE_DESKTOP`: with the
    // feature off the crate is not linked at all, so the *call* has to be gone
    // before name resolution, not merely unreached. Cargo compiles build scripts
    // with the feature cfgs, so this is the form that works for both builds — and
    // `--no-default-features` proved it by failing on the other one.
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
