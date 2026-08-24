use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// App-rooted inputs shared by local module SDK generation and binding.
#[derive(Debug, Clone)]
pub struct LocalModuleSdkInputs {
    /// Explicit root application directory.
    pub app: PathBuf,
    /// Exact module declared by the root app manifest.
    pub module: String,
    /// Explicit local directories containing dependency app directories.
    pub dependency_roots: Vec<PathBuf>,
    /// Optional nonstandard app manifest path.
    pub app_manifest: Option<PathBuf>,
    /// Optional nonstandard generated Rust source destination.
    pub source_out: Option<PathBuf>,
    /// Optional nonstandard immutable lock destination.
    pub lock: Option<PathBuf>,
}

/// Request to resolve, lock, and generate one module SDK.
#[derive(Debug, Clone)]
pub struct GenerateModuleSdkRequest {
    /// Shared app-rooted inputs.
    pub inputs: LocalModuleSdkInputs,
    /// Verify drift without writing files.
    pub check: bool,
}

/// Request to package one compiled WASM and update its app binding.
#[derive(Debug, Clone)]
pub struct BindModuleSdkRequest {
    /// Shared app-rooted inputs.
    pub inputs: LocalModuleSdkInputs,
    /// Explicit unbound compiler output.
    pub wasm: PathBuf,
    /// Optional nonstandard packaged artifact destination.
    pub bound_wasm_out: Option<PathBuf>,
    /// Verify drift without writing files.
    pub check: bool,
}

/// Resolved local module SDK build paths and immutable closure identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleSdkBuildReport {
    /// Canonical root app directory.
    pub app: PathBuf,
    /// Exact module name.
    pub module: String,
    /// Exact app manifest path.
    pub app_manifest: PathBuf,
    /// Generated Rust source path.
    pub source: PathBuf,
    /// Immutable dependency lock path.
    pub lock: PathBuf,
    /// Packaged WASM path for bind operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_wasm: Option<PathBuf>,
    /// Immutable local candidate closure digest.
    pub closure_digest: String,
    /// Whether this operation performed drift checking only.
    pub checked: bool,
}

/// Deterministic local candidate dependency lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModuleSdkLock {
    /// Resolver contract version.
    pub resolver_version: String,
    /// Root application name.
    pub root: String,
    /// Root module name.
    pub module: String,
    /// Digest of all lock fields except this digest.
    pub digest: String,
    /// Dependency-first local app closure.
    pub apps: Vec<LocalLockedApp>,
}

/// One app pinned into a local candidate dependency lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalLockedApp {
    /// Exact app manifest name.
    pub name: String,
    /// Exact app manifest version.
    pub version: String,
    /// Canonical generation-metadata digest.
    pub metadata_digest: String,
    /// Stable declared dependency names.
    pub dependencies: Vec<String>,
}
