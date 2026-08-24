//! Source-neutral immutable application bundles.

mod cache;
mod gc;
mod materialized_source;
mod provenance;
mod restore;
mod types;
mod validation;
mod verify;
mod workspace;

fn bundle_cache_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) use cache::install_canonical_bundle;
pub use cache::{
    install_local_bundle, materialize_cached_bundle, restore_local_bundle_cache_roots,
};
pub use gc::garbage_collect_local_bundle_cache;
pub(crate) use materialized_source::build_materialized_source_bundle;
pub use restore::restore_canonical_genesis_bundle_cache_roots;
pub use types::{
    BundleBlob, BundleCacheGcResult, CanonicalAppManifest, CanonicalBundleManifestV1,
    CanonicalFileManifest, InstallBundleRequest, InstallBundleResult, LocalBundleProvenance,
    LocalDependencyLock, LocalDependencyLockEntry, WorkspaceBundle,
};
pub use workspace::{build_workspace_bundle, write_workspace_lock};

pub(crate) const MAX_BUNDLE_APPS: usize = 256;
pub(crate) const MAX_BUNDLE_FILES: usize = 4096;
pub(crate) const MAX_BUNDLE_TREE_ENTRIES: usize = 8192;
pub(crate) const MAX_BUNDLE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_BUNDLE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BUNDLE_REQUEST_BYTES: usize = 96 * 1024 * 1024;
pub(crate) const MAX_BUNDLE_DEPTH: usize = 128;
