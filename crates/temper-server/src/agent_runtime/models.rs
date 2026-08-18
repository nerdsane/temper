//! Request and response models for the Agent Runtime API.

use serde::{Deserialize, Serialize};

// ── Create ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct RepoSpec {
    pub url: String,
    #[serde(default = "default_ref")]
    pub r#ref: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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

// ── Error ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ── Defaults ─────────────────────────────────────────────────────────

fn default_model() -> String {
    "claude-sonnet-4-20250514".to_string()
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
