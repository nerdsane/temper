//! Request and response models for the Agent Runtime API.

use serde::{Deserialize, Serialize};

// ── Create ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRunRequest {
    pub prompt: String,
    #[serde(default)]
    pub repo: Option<RepoSpec>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_tools")]
    pub tools: Vec<String>,
    #[serde(default = "default_sandbox_url")]
    pub sandbox_url: String,
    #[serde(default = "default_sandbox_provider")]
    pub sandbox_provider: String,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default = "default_max_turns")]
    pub max_turns: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub budget: Option<BudgetSpec>,
}

#[derive(Debug, Deserialize)]
pub struct RepoSpec {
    pub url: String,
    #[serde(default = "default_ref")]
    pub r#ref: String,
}

#[derive(Debug, Deserialize)]
pub struct BudgetSpec {
    #[serde(default = "default_max_turns")]
    pub max_turns: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct CreateRunResponse {
    pub run_id: String,
    pub status: String,
}

// ── Get ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct RunStatus {
    pub run_id: String,
    pub status: String,
    pub turn: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

// ── Steer ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SteerRequest {
    pub message: String,
}

// ── Cancel ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub run_id: String,
    pub status: String,
}

// ── Delete ───────────────────────────────────────────────────────────

/// Response returned after an agent-run deletion request is accepted.
#[derive(Debug, Serialize)]
pub struct DeleteRunResponse {
    /// The logical agent-run identifier.
    pub run_id: String,
    /// Current deletion lifecycle status, usually `Deleting`.
    pub status: String,
}

// ── Error ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ── Defaults ─────────────────────────────────────────────────────────

fn default_model() -> String {
    "claude-sonnet-4-5-20250929".to_string()
}

fn default_provider() -> String {
    "anthropic".to_string()
}

fn default_tools() -> Vec<String> {
    vec![
        "read".to_string(),
        "write".to_string(),
        "edit".to_string(),
        "bash".to_string(),
    ]
}

fn default_sandbox_url() -> String {
    "http://127.0.0.1:9999".to_string()
}

fn default_sandbox_provider() -> String {
    "local".to_string()
}

fn default_workdir() -> String {
    "/workspace".to_string()
}

fn default_max_turns() -> String {
    "20".to_string()
}

fn default_timeout() -> u64 {
    900
}

fn default_ref() -> String {
    "main".to_string()
}
