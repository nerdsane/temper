use serde::{Deserialize, Serialize};

/// One immutable file in a canonical application bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalFileManifest {
    /// Slash-separated path relative to the application root.
    pub path: String,
    /// Exact decoded byte length.
    pub size: u64,
    /// SHA-256 content digest with the `sha256:` prefix.
    pub blob_digest: String,
}

/// One application in a canonical dependency closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalAppManifest {
    /// Manifest-declared application name.
    pub name: String,
    /// Manifest-declared application version.
    pub version: String,
    /// Resolved dependency application names in sorted order.
    pub dependencies: Vec<String>,
    /// Immutable files in sorted path order.
    pub files: Vec<CanonicalFileManifest>,
}

/// Version-one source-neutral immutable bundle manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalBundleManifestV1 {
    /// Manifest schema version. Version one is the only accepted value.
    pub schema_version: u32,
    /// Root application name.
    pub root_app: String,
    /// Complete dependency closure, sorted by application name.
    pub apps: Vec<CanonicalAppManifest>,
    /// Domain-separated SHA-256 digest of the canonical manifest records.
    pub bundle_digest: String,
}

/// One transported content-addressed blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleBlob {
    /// SHA-256 content digest with the `sha256:` prefix.
    pub digest: String,
    /// Standard base64-encoded file bytes.
    pub content_base64: String,
}

/// Local-only build provenance, excluded from bundle identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBundleProvenance {
    /// Display-only workspace locator.
    pub source_locator: String,
    /// SHA-256 digest of `temper.lock.toml`, or the empty string without a lock.
    #[serde(default)]
    pub lock_digest: String,
}

/// Bounded local bundle installation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallBundleRequest {
    /// Credential-bound target tenant.
    pub tenant: String,
    /// Local build provenance.
    pub provenance: LocalBundleProvenance,
    /// Canonical manifest.
    pub manifest: CanonicalBundleManifestV1,
    /// Blobs required by the manifest. Already-cached blobs may be omitted.
    pub blobs: Vec<BundleBlob>,
}

/// Result returned after a canonical local bundle installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallBundleResult {
    /// Installed root application name.
    pub app_name: String,
    /// Pinned canonical bundle digest.
    pub bundle_digest: String,
    /// Credential-bound target tenant.
    pub tenant: String,
    /// Materialized immutable cache view.
    pub materialized_path: String,
    /// Entity types registered for the first time.
    pub added: Vec<String>,
    /// Entity types updated by the install.
    pub updated: Vec<String>,
    /// Entity types already matching the installed bundle.
    pub skipped: Vec<String>,
}

/// Explicit local dependency lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDependencyLock {
    /// Lock schema version.
    #[serde(default = "lock_version")]
    pub version: u32,
    /// Explicit local dependency sources.
    #[serde(default, rename = "local")]
    pub entries: Vec<LocalDependencyLockEntry>,
}

const fn lock_version() -> u32 {
    1
}

impl Default for LocalDependencyLock {
    fn default() -> Self {
        Self {
            version: lock_version(),
            entries: Vec::new(),
        }
    }
}

/// One explicit local dependency source and its last resolved digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDependencyLockEntry {
    /// Dependency name as declared by its `app.toml`.
    pub name: String,
    /// Path relative to the root lock file.
    pub path: String,
    /// Last successfully resolved application content digest.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
}

/// Locally built request plus the refreshed lock contents.
#[derive(Debug, Clone)]
pub struct WorkspaceBundle {
    /// Request ready for the governed HTTP installation endpoint.
    pub request: InstallBundleRequest,
    /// Resolved lock to persist after successful construction.
    pub resolved_lock: LocalDependencyLock,
    /// Root workspace directory containing the lock.
    pub workspace_root: std::path::PathBuf,
}

/// Result of a reachability-based local bundle cache collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleCacheGcResult {
    /// Whether the operation only reported collectible objects.
    pub dry_run: bool,
    /// Number of unreferenced manifests removed or identified.
    pub manifests: usize,
    /// Number of unreferenced blobs removed or identified.
    pub blobs: usize,
    /// Number of unreferenced materialized views removed or identified.
    pub views: usize,
    /// Referenced objects that could not be decoded or inspected.
    pub retained_errors: Vec<String>,
}
