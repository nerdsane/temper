//! Sandbox provider abstraction for the agent runtime.
//!
//! Defines the contract that sandbox backends (local, Tensorlake, E2B)
//! implement. The adapter service translates between this trait and the
//! backend-specific APIs.
//!
//! This module is the canonical Rust interface. The Python adapter service
//! (`os-apps/temper-agent/sandbox/sandbox_adapter.py`) is a runtime
//! implementation that exposes the same contract over HTTP.

use serde::{Deserialize, Serialize};

/// Specification for a sandbox run.
///
/// Passed to [`SandboxProvider::provision`] and
/// [`SandboxProvider::restore`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub tenant_id: String,
    pub run_id: String,
    pub image: String,
    pub repo: RepoSpec,
    pub workdir: String,
    pub cpu: u32,
    pub memory_mib: u32,
    pub timeout_seconds: u64,
    pub network_policy: NetworkPolicy,
    pub allowed_hosts: Vec<String>,
    pub checkpoint_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSpec {
    pub url: String,
    pub r#ref: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    None,
    Allowlist,
    Egress,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy::Egress
    }
}

/// Handle to a provisioned sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub provider: String,
    pub sandbox_id: String,
    pub endpoint: String,
    pub checkpoint_ref: Option<String>,
}

/// A single tool execution request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub sandbox_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub idempotency_key: String,
}

/// A streaming event from tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ToolEvent {
    Stdout { data: String },
    Stderr { data: String },
    Result { content: String, is_error: bool },
    Done { exit_code: i32 },
}

/// Checkpoint result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResult {
    pub checkpoint_ref: String,
}

/// The sandbox provider contract.
///
/// Implementations:
/// - `LocalSandboxProvider` — wraps the existing local sandbox HTTP API
/// - `TensorlakeSandboxProvider` — calls Tensorlake's REST API
///
/// The Python adapter service (`sandbox_adapter.py`) is a standalone HTTP
/// service that implements this contract at runtime, allowing the WASM
/// modules to call it without knowing which backend is active.
#[async_trait::async_trait]
pub trait SandboxProvider: Send + Sync {
    /// Provision a new sandbox.
    async fn provision(&self, spec: &RunSpec) -> Result<SandboxHandle, String>;

    /// Execute a tool request in a sandbox.
    async fn execute(&self, handle: &SandboxHandle, request: &ToolRequest) -> Result<Vec<ToolEvent>, String>;

    /// Checkpoint the sandbox workspace state.
    async fn checkpoint(&self, handle: &SandboxHandle) -> Result<CheckpointResult, String>;

    /// Restore a sandbox from a checkpoint.
    async fn restore(&self, spec: &RunSpec, checkpoint_ref: &str) -> Result<SandboxHandle, String>;

    /// Destroy a sandbox.
    async fn destroy(&self, handle: &SandboxHandle) -> Result<(), String>;
}

/// Provider type selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    Local,
    Tensorlake,
}

impl ProviderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderType::Local => "local",
            ProviderType::Tensorlake => "tensorlake",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "tensorlake" => ProviderType::Tensorlake,
            _ => ProviderType::Local,
        }
    }
}
