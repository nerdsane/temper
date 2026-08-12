//! Turn models for interaction cycles
//!
//! DST adaptation: `OTSTurn::new()` uses `sim_uuid()` for span ID generation
//! and accepts an explicit `DateTime<Utc>` timestamp.

use crate::models::{OTSDecision, OTSMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

/// One LLM interaction cycle
///
/// Contains messages and extracted decisions.
///
/// # The token-signal contract holds on every construction path
///
/// The builder setters enforce completion-side alignment as they run, and
/// deserialization enforces the same contract through [`OTSTurnWire`]: a turn
/// that arrives over the wire misaligned is a deserialization error, not a
/// stored training sample. Direct struct literals are the one remaining path,
/// and they are inside this crate's control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "OTSTurnWire")]
pub struct OTSTurn {
    /// Turn number in sequence
    pub turn_id: i32,

    /// Span ID for tracing
    pub span_id: String,

    /// Parent span ID for nested traces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    /// When turn started
    pub timestamp: DateTime<Utc>,

    /// Duration in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,

    /// Whether turn resulted in error
    #[serde(default)]
    pub error: bool,

    /// Reward assigned to this turn
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_reward: Option<f64>,

    /// Messages in this turn
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<OTSMessage>,

    /// Decisions made in this turn
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<OTSDecision>,

    /// Token IDs of the prompt exactly as the serving stack tokenized it.
    ///
    /// RL consumers train on token IDs; re-tokenizing the rendered text
    /// drifts from what the model actually saw. Populated only when the
    /// serving stack exposes them, absent otherwise.
    ///
    /// Prompt-aligned, and the only prompt-side signal on the turn: nothing
    /// else indexes into it. Prompt tokens are never trained on, so they carry
    /// neither a mask nor log probabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_token_ids: Option<Vec<u32>>,

    /// Token IDs of the completion exactly as the serving stack emitted them.
    ///
    /// The alignment anchor for the completion side: `response_mask` and
    /// `logprobs` index into this array position for position. See
    /// [`OTSTurn::validate_token_signals`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_token_ids: Option<Vec<u32>>,

    /// Per-token loss mask over the completion: `1` for model-generated
    /// tokens, `0` for tool output or otherwise injected tokens.
    ///
    /// A multi-turn completion interleaves what the model wrote with what the
    /// environment injected back into the same response segment; the mask is
    /// what tells them apart at training time. Completion-aligned: exactly one
    /// entry per `completion_token_ids` entry, each `0` or `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mask: Option<Vec<u8>>,

    /// Per-token log probabilities reported by the serving stack.
    ///
    /// Completion-aligned: exactly one entry per `completion_token_ids` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Vec<f64>>,
}

/// The deserialization shape of [`OTSTurn`].
///
/// Exists so that every parsed turn passes through
/// [`OTSTurn::validate_token_signals`]. Without it, `serde` would build the
/// struct field by field and a misaligned upload would be accepted as valid
/// RL data. Field set, defaults, and names mirror [`OTSTurn`] exactly.
#[derive(Deserialize)]
struct OTSTurnWire {
    turn_id: i32,
    span_id: String,
    #[serde(default)]
    parent_span_id: Option<String>,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    duration_ms: Option<f64>,
    #[serde(default)]
    error: bool,
    #[serde(default)]
    turn_reward: Option<f64>,
    #[serde(default)]
    messages: Vec<OTSMessage>,
    #[serde(default)]
    decisions: Vec<OTSDecision>,
    #[serde(default)]
    prompt_token_ids: Option<Vec<u32>>,
    #[serde(default)]
    completion_token_ids: Option<Vec<u32>>,
    #[serde(default)]
    response_mask: Option<Vec<u8>>,
    #[serde(default)]
    logprobs: Option<Vec<f64>>,
}

impl TryFrom<OTSTurnWire> for OTSTurn {
    type Error = String;

    fn try_from(wire: OTSTurnWire) -> Result<Self, Self::Error> {
        let turn = Self {
            turn_id: wire.turn_id,
            span_id: wire.span_id,
            parent_span_id: wire.parent_span_id,
            timestamp: wire.timestamp,
            duration_ms: wire.duration_ms,
            error: wire.error,
            turn_reward: wire.turn_reward,
            messages: wire.messages,
            decisions: wire.decisions,
            prompt_token_ids: wire.prompt_token_ids,
            completion_token_ids: wire.completion_token_ids,
            response_mask: wire.response_mask,
            logprobs: wire.logprobs,
        };
        turn.validate_token_signals()?;
        Ok(turn)
    }
}

impl OTSTurn {
    /// Create a new turn with the given ID and timestamp.
    ///
    /// Uses `sim_uuid()` for deterministic span ID generation in simulation.
    pub fn new(turn_id: i32, timestamp: DateTime<Utc>) -> Self {
        Self {
            turn_id,
            span_id: sim_uuid().to_string(),
            parent_span_id: None,
            timestamp,
            duration_ms: None,
            error: false,
            turn_reward: None,
            messages: Vec::new(),
            decisions: Vec::new(),
            prompt_token_ids: None,
            completion_token_ids: None,
            response_mask: None,
            logprobs: None,
        }
    }

    /// Set the span ID
    pub fn with_span_id(mut self, span_id: impl Into<String>) -> Self {
        self.span_id = span_id.into();
        self
    }

    /// Set the parent span ID
    pub fn with_parent_span_id(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    /// Set the duration in milliseconds
    pub fn with_duration_ms(mut self, duration_ms: f64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Mark this turn as an error
    pub fn with_error(mut self, error: bool) -> Self {
        self.error = error;
        self
    }

    /// Set the turn reward
    pub fn with_turn_reward(mut self, turn_reward: f64) -> Self {
        self.turn_reward = Some(turn_reward);
        self
    }

    /// Add a message
    pub fn with_message(mut self, message: OTSMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Add a decision
    pub fn with_decision(mut self, decision: OTSDecision) -> Self {
        self.decisions.push(decision);
        self
    }

    /// Set all messages
    pub fn with_messages(mut self, messages: Vec<OTSMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Set all decisions
    pub fn with_decisions(mut self, decisions: Vec<OTSDecision>) -> Self {
        self.decisions = decisions;
        self
    }

    /// Set the prompt token IDs
    pub fn with_prompt_token_ids(mut self, prompt_token_ids: Vec<u32>) -> Self {
        self.prompt_token_ids = Some(prompt_token_ids);
        self
    }

    /// Set the completion token IDs.
    ///
    /// Panics if a completion-aligned signal is already set at a different
    /// length.
    pub fn with_completion_token_ids(mut self, completion_token_ids: Vec<u32>) -> Self {
        self.assert_completion_aligned("completion_token_ids", completion_token_ids.len());
        self.completion_token_ids = Some(completion_token_ids);
        self
    }

    /// Set the per-token response mask (`1` = model token, `0` = injected token).
    ///
    /// Panics if any entry is outside `{0, 1}`, or if a completion-aligned
    /// signal is already set at a different length.
    pub fn with_response_mask(mut self, response_mask: Vec<u8>) -> Self {
        if let Some(value) = response_mask.iter().find(|value| **value > 1) {
            panic!("response_mask entries must be 0 or 1, got {value}");
        }
        self.assert_completion_aligned("response_mask", response_mask.len());
        self.response_mask = Some(response_mask);
        self
    }

    /// Set the per-token log probabilities.
    ///
    /// Panics if a completion-aligned signal is already set at a different
    /// length.
    pub fn with_logprobs(mut self, logprobs: Vec<f64>) -> Self {
        self.assert_completion_aligned("logprobs", logprobs.len());
        self.logprobs = Some(logprobs);
        self
    }

    /// Check that the completion-side signals agree.
    ///
    /// The setters catch every pairwise length disagreement whatever order
    /// they are called in, but they cannot see a signal that is never set:
    /// a mask or a logprob array with no `completion_token_ids` has nothing
    /// to index into and is meaningless. This is the whole-turn check, and it
    /// runs on three paths: [`TrajectoryBuilder::end_turn`](crate::TrajectoryBuilder)
    /// before a turn is sealed, deserialization of any turn that arrives over
    /// the wire, and any caller that wants to check a hand-built turn.
    pub fn validate_token_signals(&self) -> Result<(), String> {
        if let Some(response_mask) = &self.response_mask
            && let Some(value) = response_mask.iter().find(|value| **value > 1)
        {
            return Err(format!(
                "turn {} has a response_mask entry of {value}; the mask is a per-token loss \
                 switch and every entry is 0 or 1",
                self.turn_id
            ));
        }
        let completion = self.completion_token_ids.as_ref().map(Vec::len);
        for (name, len) in [
            ("response_mask", self.response_mask.as_ref().map(Vec::len)),
            ("logprobs", self.logprobs.as_ref().map(Vec::len)),
        ] {
            let Some(len) = len else { continue };
            match completion {
                None => {
                    return Err(format!(
                        "turn {} carries {name} with no completion_token_ids to align it against",
                        self.turn_id
                    ));
                }
                Some(completion) if completion != len => {
                    return Err(format!(
                        "turn {} has {len} {name} entries but {completion} completion_token_ids; \
                         completion-side signals are aligned position for position",
                        self.turn_id
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Panic when `len` disagrees with a completion-aligned signal that is
    /// already set. Order-independent: whichever setter runs last sees at
    /// least one counterpart and trips.
    fn assert_completion_aligned(&self, name: &str, len: usize) {
        for (other_name, other_len) in [
            (
                "completion_token_ids",
                self.completion_token_ids.as_ref().map(Vec::len),
            ),
            ("response_mask", self.response_mask.as_ref().map(Vec::len)),
            ("logprobs", self.logprobs.as_ref().map(Vec::len)),
        ] {
            if let Some(other_len) = other_len
                && other_len != len
            {
                panic!(
                    "{name} has {len} entries but {other_name} has {other_len}; \
                     completion-side signals are aligned position for position"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSMessageContent};
    use temper_runtime::scheduler::sim_now;

    #[test]
    fn test_turn_serialization() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp)
            .with_duration_ms(150.5)
            .with_turn_reward(0.85);

        let json_str = serde_json::to_string(&turn).unwrap();
        let parsed: OTSTurn = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.turn_id, 1);
        assert_eq!(parsed.duration_ms, Some(150.5));
        assert_eq!(parsed.turn_reward, Some(0.85));
        assert!(!parsed.error);
    }

    #[test]
    fn test_turn_with_messages_and_decisions() {
        let timestamp = sim_now();
        let message = OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Hello"),
            timestamp,
        );
        let decision = OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new("search"),
            OTSConsequence::success(),
        );

        let turn = OTSTurn::new(1, timestamp)
            .with_message(message)
            .with_decision(decision);

        assert_eq!(turn.messages.len(), 1);
        assert_eq!(turn.decisions.len(), 1);

        let json_str = serde_json::to_string(&turn).unwrap();
        let parsed: OTSTurn = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.decisions.len(), 1);
    }

    #[test]
    fn test_turn_minimal() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp);

        let json_str = serde_json::to_string(&turn).unwrap();

        // Optional fields should not appear
        assert!(!json_str.contains("\"parent_span_id\""));
        assert!(!json_str.contains("\"duration_ms\""));
        assert!(!json_str.contains("\"turn_reward\""));

        // Empty vectors should not appear
        assert!(!json_str.contains("\"messages\""));
        assert!(!json_str.contains("\"decisions\""));

        // Error defaults to false but should appear
        assert!(json_str.contains("\"error\":false"));
    }

    #[test]
    fn test_turn_with_error() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp).with_error(true);

        assert!(turn.error);

        let json_str = serde_json::to_string(&turn).unwrap();
        assert!(json_str.contains("\"error\":true"));
    }

    #[test]
    fn completion_side_signals_must_agree_in_length() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp)
            .with_completion_token_ids(vec![4, 5])
            .with_response_mask(vec![1, 0])
            .with_logprobs(vec![-0.1, -0.2]);
        assert!(turn.validate_token_signals().is_ok());
    }

    #[test]
    #[should_panic(expected = "response_mask has 3 entries but completion_token_ids has 2")]
    fn a_mask_longer_than_the_completion_panics() {
        let timestamp = sim_now();
        OTSTurn::new(1, timestamp)
            .with_completion_token_ids(vec![4, 5])
            .with_response_mask(vec![1, 0, 1]);
    }

    #[test]
    #[should_panic(expected = "completion_token_ids has 2 entries but response_mask has 3")]
    fn the_alignment_check_does_not_depend_on_setter_order() {
        let timestamp = sim_now();
        OTSTurn::new(1, timestamp)
            .with_response_mask(vec![1, 0, 1])
            .with_completion_token_ids(vec![4, 5]);
    }

    #[test]
    #[should_panic(expected = "response_mask entries must be 0 or 1, got 2")]
    fn a_mask_outside_the_zero_one_domain_panics() {
        let timestamp = sim_now();
        OTSTurn::new(1, timestamp).with_response_mask(vec![1, 2]);
    }

    #[test]
    fn a_signal_with_nothing_to_align_against_is_rejected_by_validation() {
        let timestamp = sim_now();
        // The setters cannot catch this: there is no counterpart to compare
        // against, so only the whole-turn check sees it.
        let turn = OTSTurn::new(1, timestamp).with_response_mask(vec![1, 0]);
        let error = turn
            .validate_token_signals()
            .expect_err("a mask with no completion tokens is meaningless");
        assert!(error.contains("no completion_token_ids"), "{error}");
    }

    #[test]
    fn a_turn_with_no_token_signals_validates() {
        let timestamp = sim_now();
        assert!(
            OTSTurn::new(1, timestamp).validate_token_signals().is_ok(),
            "token signals are optional; absent is not misaligned"
        );
    }

    #[test]
    fn deserialization_rejects_a_misaligned_turn() {
        let json = serde_json::json!({
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "completion_token_ids": [4, 5],
            "logprobs": [-0.1, -0.2, -0.3],
        });
        let error = serde_json::from_value::<OTSTurn>(json)
            .expect_err("a turn with three logprobs over two tokens is not valid RL data");
        assert!(error.to_string().contains("logprobs"), "{error}");
    }

    #[test]
    fn deserialization_rejects_a_signal_with_nothing_to_align_against() {
        let json = serde_json::json!({
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "response_mask": [1, 0],
        });
        let error = serde_json::from_value::<OTSTurn>(json)
            .expect_err("a mask with no completion tokens indexes into nothing");
        assert!(
            error.to_string().contains("no completion_token_ids"),
            "{error}"
        );
    }

    #[test]
    fn deserialization_rejects_a_mask_outside_the_zero_one_domain() {
        let json = serde_json::json!({
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "completion_token_ids": [4, 5],
            "response_mask": [1, 7],
        });
        let error = serde_json::from_value::<OTSTurn>(json)
            .expect_err("a mask entry of 7 is not a loss switch");
        assert!(error.to_string().contains("0 or 1"), "{error}");
    }

    #[test]
    fn an_aligned_turn_round_trips_through_serde() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp)
            .with_completion_token_ids(vec![4, 5])
            .with_response_mask(vec![1, 0])
            .with_logprobs(vec![-0.1, -0.2]);
        let json = serde_json::to_string(&turn).expect("serialize");
        let parsed: OTSTurn = serde_json::from_str(&json).expect("aligned turns deserialize");
        assert_eq!(parsed, turn);
    }

    #[test]
    fn test_turn_with_parent_span() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp).with_parent_span_id("parent-span-123");

        assert_eq!(turn.parent_span_id, Some("parent-span-123".to_string()));
    }
}
