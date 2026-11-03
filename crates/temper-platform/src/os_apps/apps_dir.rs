//! Where the static apps folder is.

use std::path::PathBuf;

/// The static apps folder: `TEMPER_OS_APPS_DIR`, then the workspace's
/// `os-apps/` (dev and `cargo test`), then `./os-apps/`. `None` when there
/// is none.
pub(super) fn static_apps_dir() -> Option<PathBuf> {
    // Priority 1: TEMPER_OS_APPS_DIR env var.
    if let Ok(dir) = std::env::var("TEMPER_OS_APPS_DIR") {
        // determinism-ok: env var read at startup for configuration
        let path = PathBuf::from(dir);
        if path.is_dir() {
            tracing::info!(
                "Loading OS apps from TEMPER_OS_APPS_DIR: {}",
                path.display()
            );
            return Some(path);
        }
    }

    // Priority 2: Relative to this crate's source (works in dev and cargo test).
    let compile_time_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("os-apps");
    if compile_time_dir.is_dir() {
        let canonical = compile_time_dir
            .canonicalize()
            .unwrap_or(compile_time_dir.clone());
        tracing::info!("Loading OS apps from workspace: {}", canonical.display());
        return Some(canonical);
    }

    // Priority 3: ./os-apps/ relative to CWD.
    let cwd_dir = PathBuf::from("os-apps");
    if cwd_dir.is_dir() {
        let canonical = cwd_dir.canonicalize().unwrap_or(cwd_dir.clone());
        tracing::info!("Loading OS apps from CWD: {}", canonical.display());
        return Some(canonical);
    }

    tracing::warn!("No os-apps directory found. Set TEMPER_OS_APPS_DIR for dev/local apps.");
    None
}
