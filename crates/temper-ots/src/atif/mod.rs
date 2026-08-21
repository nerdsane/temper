//! Export an [`OTSTrajectory`] as an ATIF v1.7 document.
//!
//! ATIF (Agent Trajectory Interchange Format) is the Harbor project's
//! interchange format for agent trajectories, specified in
//! `rfcs/0001-trajectory-format.md` of `github.com/harbor-framework/harbor`.
//! Exporting to it makes Temper trajectories readable by the SFT, RL, and
//! replay tooling built around that format.
//!
//! # Shape of the mapping
//!
//! ATIF is a flat list of steps; OTS is a list of turns, each holding its own
//! messages and decisions. One OTS turn becomes at most one ATIF **agent**
//! step, preceded by one **user** or **system** step per user/system message
//! inside that turn. The trajectory's `system_message` becomes the first step.
//!
//! # Mapping table
//!
//! ## Lossless — carried in ATIF-native fields
//!
//! | OTS | ATIF |
//! | --- | --- |
//! | `trajectory_id` | `trajectory_id` |
//! | `system_message.content` / `.timestamp` | leading step, `source = "system"` |
//! | `turns[].timestamp` | `steps[].timestamp` |
//! | user-role message text | step with `source = "user"` |
//! | system-role message text | step with `source = "system"` |
//! | assistant-role text messages | agent step `message` (joined with newlines) |
//! | assistant `reasoning` | `steps[].reasoning_content` |
//! | invoking `decisions[].choice.action` | `tool_calls[].function_name` |
//! | invoking `decisions[].choice.arguments` | `tool_calls[].arguments` |
//! | invoking `decisions[].cause_id` (else `decision_id`) | `tool_calls[].tool_call_id` and the matching `observation.results[].source_call_id` |
//! | invoking `decisions[].consequence.result_summary` | `observation.results[].content` |
//! | tool-role message content | `observation.results[]` with no `source_call_id` (OTS does not link tool messages to a call id) |
//! | `turns[].prompt_token_ids` / `completion_token_ids` / `logprobs` | `metrics` fields of the same name |
//! | `metadata.harness` (else `framework`, else `agent_id`) | `agent.name` |
//! | `metadata.agent_version` (else [`UNKNOWN_AGENT_VERSION`]) | `agent.version` |
//!
//! "Invoking" means a decision whose type reports
//! [`DecisionType::is_invocation`](crate::models::DecisionType::is_invocation):
//! a tool selection or a parameter choice. A reasoning step or a response
//! formulation names a thought rather than a callable, so it produces no tool
//! call and no observation result — only its `temper.decisions` entry.
//!
//! ## Derived
//!
//! | ATIF | Derivation |
//! | --- | --- |
//! | `schema_version` | the constant [`ATIF_SCHEMA_VERSION`] |
//! | `metrics.prompt_tokens` / `completion_tokens` | length of the matching token-id array; absent when the array is absent, because OTS records no token counts of its own |
//! | `final_metrics.total_prompt_tokens` / `total_completion_tokens` | sum of the per-step derived counts; absent when no turn carried token ids |
//! | `final_metrics.total_steps` | number of emitted steps |
//! | `session_id` | supplied by the caller — OTS holds the session on the storage row, not in the document |
//!
//! ## Carried in `extra` (OTS-only value ATIF has no field for)
//!
//! | Key | Location | Content |
//! | --- | --- | --- |
//! | `temper.ots_version` | root `extra` | the OTS document `version` |
//! | `temper.metadata` | root `extra` | the whole `OTSMetadata` verbatim, including `spec_version`, `harness`, `agent_version`, `outcome`, `feedback_score`, `human_reviewed`, `tags`, `domain`, `environment`, `parent_trajectory_id` |
//! | `temper.context` | root `extra` | the `OTSContext` verbatim, when non-empty |
//! | `temper.final_reward` | root `extra` | `final_reward` |
//! | `temper.agent_id` / `temper.framework` / `temper.harness` / `temper.spec_version` | `agent.extra` | the originals, so the `agent.name` fallback chain stays reversible and the governing spec stays readable without being mistaken for the agent release |
//! | `temper.turn_id` / `temper.span_id` / `temper.parent_span_id` / `temper.duration_ms` / `temper.error` | step `extra` | per-turn identity and outcome |
//! | `temper.decisions` | step `extra` | each decision verbatim: `decision_type`, `state`, `alternatives`, `evaluation`, `credit_assignment`, `embedding`, and the choice `rationale` / `confidence` — ATIF's `ToolCallSchema` carries none of it |
//! | `temper.assistant_content` | step `extra` | assistant message payloads that are not plain text (tool-call, tool-response, widget content) |
//! | `temper.consequence` | `observation.results[].extra` | `success` and `error_type` of the decision that produced the result |
//! | `temper.response_mask` / `temper.turn_reward` | `metrics.extra` | the per-token loss mask (completion-aligned, same length as `completion_token_ids`) and the per-turn reward |
//!
//! ## Cannot round-trip
//!
//! - **Model identity.** ATIF has `agent.model_name` and `steps[].model_name`;
//!   OTS records no model anywhere, so both are always absent.
//! - **Cost and cache accounting.** `metrics.cost_usd`, `cached_tokens`, and
//!   the `final_metrics` cost totals have no OTS source.
//! - **Subagent embedding.** ATIF nests children under the parent's
//!   `subagent_trajectories`; OTS points the other way, from child to parent
//!   via `metadata.parent_trajectory_id`. A single OTS document therefore
//!   exports with `subagent_trajectories` absent, and the parent link is kept
//!   under `temper.metadata`. Reassembling a hierarchy needs the sibling
//!   documents, which one export does not have.
//! - **Message identity.** OTS `message_id`, `visibility`, and
//!   `context_snapshot` describe individual messages; ATIF steps have no
//!   per-message identity to hang them on, so they are not exported.
//! - **Token counts without ids.** A turn that reports token counts but no
//!   token ids cannot exist in OTS — counts are derived from ids — so an ATIF
//!   document imported into OTS and exported again loses any count whose ids
//!   were absent.
//! - **Context compaction.** ATIF links trajectory segments across a
//!   summarization boundary with `continued_trajectory_ref` and marks carried
//!   steps `is_copied_context`. OTS models neither, so both are always absent;
//!   a Temper session that compacts its context records no boundary for the
//!   export to point at.

mod types;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

pub use types::{
    ATIF_SCHEMA_VERSION, AtifAgent, AtifFinalMetrics, AtifMetrics, AtifObservation,
    AtifObservationResult, AtifSource, AtifStep, AtifToolCall, AtifTrajectory,
};

use crate::models::{
    ContentType, MessageRole, OTSDecision, OTSMessage, OTSMessageContent, OTSTrajectory, OTSTurn,
};

/// `agent.version` for a trajectory whose producer did not report the agent
/// system's release.
///
/// ATIF requires the field. OTS only knows it when the harness fills
/// `metadata.agent_version`, and no other OTS value answers the question — the
/// spec hash identifies the governing spec and the document `version`
/// identifies the OTS format, so substituting either would tell a consumer
/// that unrelated builds are the same release.
pub const UNKNOWN_AGENT_VERSION: &str = "unknown";

/// Why an OTS trajectory could not be exported as ATIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtifExportError {
    /// The trajectory produced no steps.
    ///
    /// ATIF v1.7 models a trajectory as its step list, and a document with an
    /// empty one describes no run. It happens for a session finalized before
    /// anything ran: no system message, no turn that carried a message, a
    /// decision, or token metrics.
    NoSteps,
}

impl std::fmt::Display for AtifExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSteps => write!(
                f,
                "the trajectory produced no ATIF steps; ATIF v1.7 requires at least one, and a \
                 run with no system message, no messages, no decisions, and no token metrics has \
                 nothing to export"
            ),
        }
    }
}

impl std::error::Error for AtifExportError {}

/// Export an OTS trajectory as an ATIF v1.7 document.
///
/// `session_id` is supplied by the caller because OTS keeps the session on the
/// storage row rather than in the document. Pass `None` when the session is
/// unknown; ATIF v1.7 makes `session_id` optional.
///
/// Fails with [`AtifExportError::NoSteps`] when the trajectory yields no
/// steps: a stepless document passes Rust's type checker and fails Harbor's
/// validator, so it is refused here rather than handed on as valid ATIF.
///
/// See the module docs for the full field-by-field mapping.
pub fn to_atif(
    trajectory: &OTSTrajectory,
    session_id: Option<&str>,
) -> Result<AtifTrajectory, AtifExportError> {
    let mut steps: Vec<AtifStep> = Vec::new();
    let mut next_step_id: i64 = 1;

    if let Some(system_message) = &trajectory.system_message {
        steps.push(AtifStep {
            step_id: next_step_id,
            timestamp: Some(format_timestamp(system_message.timestamp)),
            source: AtifSource::System,
            message: system_message.content.clone(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            observation: None,
            metrics: None,
            extra: BTreeMap::new(),
        });
        next_step_id += 1;
    }

    for turn in &trajectory.turns {
        for message in &turn.messages {
            let source = match message.role {
                MessageRole::User => AtifSource::User,
                MessageRole::System => AtifSource::System,
                MessageRole::Assistant | MessageRole::Tool => continue,
            };
            steps.push(AtifStep {
                step_id: next_step_id,
                timestamp: Some(format_timestamp(message.timestamp)),
                source,
                message: plain_text(&message.content).unwrap_or_default(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                observation: None,
                metrics: None,
                extra: turn_identity(turn),
            });
            next_step_id += 1;
        }

        if let Some(step) = agent_step(turn, next_step_id) {
            steps.push(step);
            next_step_id += 1;
        }
    }

    if steps.is_empty() {
        return Err(AtifExportError::NoSteps);
    }

    let mut extra = BTreeMap::new();
    insert_json(&mut extra, "temper.ots_version", &trajectory.version);
    insert_json(&mut extra, "temper.metadata", &trajectory.metadata);
    if trajectory.context != crate::models::OTSContext::new() {
        insert_json(&mut extra, "temper.context", &trajectory.context);
    }
    if let Some(final_reward) = trajectory.final_reward {
        insert_json(&mut extra, "temper.final_reward", &final_reward);
    }

    Ok(AtifTrajectory {
        schema_version: ATIF_SCHEMA_VERSION.to_string(),
        session_id: session_id.map(str::to_string),
        trajectory_id: Some(trajectory.trajectory_id.clone()),
        agent: agent_block(trajectory),
        final_metrics: final_metrics(&steps),
        subagent_trajectories: None,
        extra,
        steps,
    })
}

fn agent_block(trajectory: &OTSTrajectory) -> AtifAgent {
    let metadata = &trajectory.metadata;
    let name = metadata
        .harness
        .clone()
        .or_else(|| metadata.framework.clone())
        .unwrap_or_else(|| metadata.agent_id.clone());
    // ATIF's `agent.version` is the version of the agent system. The spec hash
    // and the OTS document version answer different questions, so neither
    // stands in for it: a consumer grouping runs by agent release would split
    // one build across specs and merge unrelated builds that share one.
    let version = metadata
        .agent_version
        .clone()
        .unwrap_or_else(|| UNKNOWN_AGENT_VERSION.to_string());

    let mut extra = BTreeMap::new();
    insert_json(&mut extra, "temper.agent_id", &metadata.agent_id);
    if let Some(framework) = &metadata.framework {
        insert_json(&mut extra, "temper.framework", framework);
    }
    if let Some(harness) = &metadata.harness {
        insert_json(&mut extra, "temper.harness", harness);
    }
    if let Some(spec_version) = &metadata.spec_version {
        insert_json(&mut extra, "temper.spec_version", spec_version);
    }

    AtifAgent {
        name,
        version,
        model_name: None,
        extra,
    }
}

/// Build the agent step for one turn, or `None` when the turn holds nothing an
/// agent step would carry (a turn of user messages only).
fn agent_step(turn: &OTSTurn, step_id: i64) -> Option<AtifStep> {
    let assistant_messages: Vec<&OTSMessage> = turn
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    let tool_messages: Vec<&OTSMessage> = turn
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();

    // Only invoking decisions describe an environment interaction; the rest
    // name a thought and are carried in `temper.decisions` alone.
    let invocations: Vec<&OTSDecision> = turn
        .decisions
        .iter()
        .filter(|decision| decision.decision_type.is_invocation())
        .collect();

    let metrics = turn_metrics(turn);
    let has_agent_content = !assistant_messages.is_empty()
        || !tool_messages.is_empty()
        || !turn.decisions.is_empty()
        || metrics.is_some();
    if !has_agent_content {
        return None;
    }

    let message = assistant_messages
        .iter()
        .filter_map(|m| plain_text(&m.content))
        .collect::<Vec<String>>()
        .join("\n");

    let reasoning_content = assistant_messages
        .iter()
        .filter_map(|m| m.reasoning.clone())
        .find(|reasoning| !reasoning.is_empty());

    let tool_calls: Vec<AtifToolCall> = invocations
        .iter()
        .map(|decision| AtifToolCall {
            tool_call_id: tool_call_id(decision),
            function_name: decision.choice.action.clone(),
            arguments: object_arguments(decision.choice.arguments.as_ref()),
        })
        .collect();

    let mut results: Vec<AtifObservationResult> = invocations
        .iter()
        .map(|decision| {
            let mut extra = BTreeMap::new();
            insert_json(
                &mut extra,
                "temper.consequence",
                &serde_json::json!({
                    "success": decision.consequence.success,
                    "error_type": decision.consequence.error_type,
                }),
            );
            AtifObservationResult {
                source_call_id: Some(tool_call_id(decision)),
                content: decision.consequence.result_summary.clone(),
                extra,
            }
        })
        .collect();
    results.extend(tool_messages.iter().map(|message| AtifObservationResult {
        // OTS tool messages carry no tool-call linkage, so the result stays
        // unattributed rather than guessing a call id by position.
        source_call_id: None,
        content: any_content(&message.content),
        extra: BTreeMap::new(),
    }));

    let mut extra = turn_identity(turn);
    if !turn.decisions.is_empty() {
        insert_json(&mut extra, "temper.decisions", &turn.decisions);
    }
    let assistant_content: Vec<&OTSMessageContent> = assistant_messages
        .iter()
        .map(|m| &m.content)
        .filter(|content| content.content_type != ContentType::Text)
        .collect();
    if !assistant_content.is_empty() {
        insert_json(&mut extra, "temper.assistant_content", &assistant_content);
    }

    Some(AtifStep {
        step_id,
        timestamp: Some(format_timestamp(turn.timestamp)),
        source: AtifSource::Agent,
        message,
        reasoning_content,
        tool_calls,
        observation: (!results.is_empty()).then_some(AtifObservation { results }),
        metrics,
        extra,
    })
}

fn turn_metrics(turn: &OTSTurn) -> Option<AtifMetrics> {
    let mut extra = BTreeMap::new();
    if let Some(response_mask) = &turn.response_mask {
        insert_json(&mut extra, "temper.response_mask", response_mask);
    }
    if let Some(turn_reward) = turn.turn_reward {
        insert_json(&mut extra, "temper.turn_reward", &turn_reward);
    }

    let metrics = AtifMetrics {
        prompt_tokens: turn.prompt_token_ids.as_ref().map(|ids| ids.len() as u64),
        completion_tokens: turn
            .completion_token_ids
            .as_ref()
            .map(|ids| ids.len() as u64),
        cost_usd: None,
        prompt_token_ids: turn.prompt_token_ids.clone(),
        completion_token_ids: turn.completion_token_ids.clone(),
        logprobs: turn.logprobs.clone(),
        extra,
    };
    (!metrics.is_empty()).then_some(metrics)
}

fn final_metrics(steps: &[AtifStep]) -> Option<AtifFinalMetrics> {
    let mut total_prompt_tokens: Option<u64> = None;
    let mut total_completion_tokens: Option<u64> = None;
    for metrics in steps.iter().filter_map(|step| step.metrics.as_ref()) {
        if let Some(prompt_tokens) = metrics.prompt_tokens {
            total_prompt_tokens = Some(total_prompt_tokens.unwrap_or(0) + prompt_tokens);
        }
        if let Some(completion_tokens) = metrics.completion_tokens {
            total_completion_tokens =
                Some(total_completion_tokens.unwrap_or(0) + completion_tokens);
        }
    }

    let final_metrics = AtifFinalMetrics {
        total_prompt_tokens,
        total_completion_tokens,
        total_cost_usd: None,
        total_steps: Some(steps.len() as u64),
        extra: BTreeMap::new(),
    };
    Some(final_metrics)
}

/// The id linking a decision to the observation it produced.
///
/// `cause_id` is the provider's tool-call id and is preferred; `decision_id`
/// is the fallback so the correlation still holds for trajectories recorded
/// before causality capture existed.
fn tool_call_id(decision: &OTSDecision) -> String {
    decision
        .cause_id
        .clone()
        .unwrap_or_else(|| decision.decision_id.clone())
}

/// ATIF requires `arguments` to be a JSON object.
fn object_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    match arguments {
        Some(value) if value.is_object() => value.clone(),
        // A non-object argument payload is wrapped rather than dropped, so the
        // export stays schema-valid without losing what the agent passed.
        Some(value) => serde_json::json!({ "temper.value": value }),
        None => serde_json::json!({}),
    }
}

/// Text of a plain-text message content, or `None` for structured content.
fn plain_text(content: &OTSMessageContent) -> Option<String> {
    match content.content_type {
        ContentType::Text => content.text.clone(),
        _ => None,
    }
}

/// Text of any message content: the text field when present, else the
/// structured payload serialized as JSON.
fn any_content(content: &OTSMessageContent) -> Option<String> {
    content
        .text
        .clone()
        .or_else(|| content.data.as_ref().map(|data| data.to_string()))
}

fn turn_identity(turn: &OTSTurn) -> BTreeMap<String, serde_json::Value> {
    let mut extra = BTreeMap::new();
    insert_json(&mut extra, "temper.turn_id", &turn.turn_id);
    insert_json(&mut extra, "temper.span_id", &turn.span_id);
    if let Some(parent_span_id) = &turn.parent_span_id {
        insert_json(&mut extra, "temper.parent_span_id", parent_span_id);
    }
    if let Some(duration_ms) = turn.duration_ms {
        insert_json(&mut extra, "temper.duration_ms", &duration_ms);
    }
    if turn.error {
        insert_json(&mut extra, "temper.error", &true);
    }
    extra
}

/// Insert a serializable value into an `extra` map.
///
/// Serialization of OTS types cannot fail — every field is a plain data type
/// with no map keys other than strings — so a failure is dropped rather than
/// propagated as an error the caller could not act on.
fn insert_json<T: Serialize>(
    extra: &mut BTreeMap<String, serde_json::Value>,
    key: &str,
    value: &T,
) {
    if let Ok(value) = serde_json::to_value(value) {
        extra.insert(key.to_string(), value);
    }
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339()
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
