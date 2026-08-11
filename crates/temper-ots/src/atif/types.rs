//! The ATIF v1.7 document schema as Rust types.
//!
//! Field semantics follow the Harbor RFC; the OTS-to-ATIF mapping that fills
//! them lives in the parent module.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The ATIF schema version this exporter emits.
pub const ATIF_SCHEMA_VERSION: &str = "ATIF-v1.7";

/// Originator of an ATIF step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtifSource {
    /// A system prompt or system-initiated event.
    System,
    /// A message from the user.
    User,
    /// An agent turn: response, tool calls, and their observations.
    Agent,
}

/// An ATIF v1.7 trajectory document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifTrajectory {
    /// ATIF compatibility marker, always [`ATIF_SCHEMA_VERSION`].
    pub schema_version: String,

    /// Run-scoped identifier, supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Document-scoped identifier, from the OTS trajectory id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,

    /// The agent system that produced the trajectory.
    pub agent: AtifAgent,

    /// The full interaction history.
    pub steps: Vec<AtifStep>,

    /// Aggregate statistics for the whole trajectory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<AtifFinalMetrics>,

    /// Embedded subagent trajectories. Always absent on an OTS export — see
    /// the module docs on subagent embedding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_trajectories: Option<Vec<AtifTrajectory>>,

    /// Trajectory-level values ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// The agent system identification block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifAgent {
    /// Name of the agent system.
    pub name: String,

    /// Version identifier of the agent system.
    pub version: String,

    /// Default model for the run. Never set on an OTS export — OTS records no
    /// model identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,

    /// Agent-level values ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One step of the interaction history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifStep {
    /// Ordinal index, starting at 1.
    pub step_id: i64,

    /// ISO-8601 timestamp of the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Who produced the step.
    pub source: AtifSource,

    /// The dialogue message. Required by ATIF, may be empty.
    pub message: String,

    /// The agent's explicit internal reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,

    /// Tool invocations issued by this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AtifToolCall>,

    /// Environment feedback for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,

    /// LLM operational and RL data for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AtifMetrics>,

    /// Step-level values ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// A single tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifToolCall {
    /// Identifier correlating this call with its observation result.
    pub tool_call_id: String,

    /// The invoked function or action.
    pub function_name: String,

    /// Arguments passed to the function. Always a JSON object.
    pub arguments: serde_json::Value,
}

/// Environment feedback for a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifObservation {
    /// One entry per tool call or action result.
    pub results: Vec<AtifObservationResult>,
}

/// A single observation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtifObservationResult {
    /// The `tool_call_id` this result answers, when the result came from a
    /// structured tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,

    /// The result payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Result-level values ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Per-step LLM metrics.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AtifMetrics {
    /// Input token count. Derived from `prompt_token_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,

    /// Output token count. Derived from `completion_token_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,

    /// Monetary cost. Never set on an OTS export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,

    /// Prompt token ids exactly as the serving stack produced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<u32>>,

    /// Completion token ids exactly as the serving stack produced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_token_ids: Option<Vec<u32>>,

    /// Per-token log probabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<f64>>,

    /// Metric-level values ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Aggregate statistics for the whole trajectory.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AtifFinalMetrics {
    /// Sum of the derived per-step prompt token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<u64>,

    /// Sum of the derived per-step completion token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<u64>,

    /// Total cost. Never set on an OTS export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,

    /// Number of emitted steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<u64>,

    /// Trajectory-level metrics ATIF has no field for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl AtifMetrics {
    pub(super) fn is_empty(&self) -> bool {
        self.prompt_tokens.is_none()
            && self.completion_tokens.is_none()
            && self.cost_usd.is_none()
            && self.prompt_token_ids.is_none()
            && self.completion_token_ids.is_none()
            && self.logprobs.is_none()
            && self.extra.is_empty()
    }
}
