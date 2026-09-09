//! Incremental trajectory builder
//!
//! Provides a [`TrajectoryBuilder`] that accumulates turns incrementally,
//! suitable for capturing trajectories as they unfold during agent execution.

use crate::models::{
    OTSContext, OTSDecision, OTSMessage, OTSMetadata, OTSSystemMessage, OTSTrajectory, OTSTurn,
};
use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;

/// Incremental builder for constructing trajectories turn by turn.
///
/// # Example
///
/// ```rust,ignore
/// use temper_ots::{TrajectoryBuilder, OTSMetadata, OutcomeType, OTSMessage, MessageRole, OTSMessageContent};
/// use temper_runtime::scheduler::sim_now;
///
/// let now = sim_now();
/// let metadata = OTSMetadata::new("task", "agent", OutcomeType::Success, now);
/// let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());
///
/// builder.start_turn(now);
/// builder.add_message(OTSMessage::new(MessageRole::User, OTSMessageContent::text("Hello"), now));
/// builder.end_turn(now);
///
/// let trajectory = builder.build();
/// ```
#[derive(Clone)]
pub struct TrajectoryBuilder {
    /// Trajectory metadata
    metadata: OTSMetadata,
    /// Initial context
    context: OTSContext,
    /// Optional system message
    system_message: Option<OTSSystemMessage>,
    /// Completed turns
    turns: Vec<OTSTurn>,
    /// Turn currently being built (if any)
    current_turn: Option<OTSTurn>,
}

impl TrajectoryBuilder {
    /// Create a new builder with required metadata and context.
    pub fn new(metadata: OTSMetadata, context: OTSContext) -> Self {
        Self {
            metadata,
            context,
            system_message: None,
            turns: Vec::new(),
            current_turn: None,
        }
    }

    /// Start a new turn. Panics if a turn is already in progress.
    ///
    /// The turn ID is automatically assigned based on the number of
    /// completed turns.
    pub fn start_turn(&mut self, timestamp: DateTime<Utc>) {
        assert!(
            self.current_turn.is_none(),
            "Cannot start a new turn while one is in progress"
        );
        let turn_id = (self.turns.len() + 1) as i32;
        self.current_turn = Some(OTSTurn::new(turn_id, timestamp));
    }

    /// Add a message to the current turn. Panics if no turn is in progress.
    pub fn add_message(&mut self, message: OTSMessage) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot add message: no turn in progress");
        turn.messages.push(message);
    }

    /// Add a decision to the current turn. Panics if no turn is in progress.
    pub fn add_decision(&mut self, decision: OTSDecision) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot add decision: no turn in progress");
        turn.decisions.push(decision);
    }

    /// End the current turn, recording its duration.
    ///
    /// Duration is computed as the difference between `end_time` and the
    /// turn's start timestamp.
    ///
    /// Panics if no turn is in progress, or if the turn's completion-side
    /// token signals do not line up — a misaligned turn is a malformed
    /// training sample, and sealing it here is the last point at which the
    /// producer can still be told which turn was wrong.
    pub fn end_turn(&mut self, end_time: DateTime<Utc>) {
        let mut turn = self
            .current_turn
            .take()
            .expect("Cannot end turn: no turn in progress");
        if let Err(error) = turn.validate_token_signals() {
            panic!("Cannot end turn: {error}");
        }
        let duration_ms = (end_time - turn.timestamp).num_milliseconds() as f64;
        turn.duration_ms = Some(duration_ms);
        self.turns.push(turn);
    }

    /// Set the system message for the trajectory.
    pub fn set_system_message(&mut self, system_message: OTSSystemMessage) {
        self.system_message = Some(system_message);
    }

    /// Record the actor spec version (hash or version) this run executed under.
    pub fn set_spec_version(&mut self, spec_version: impl Into<String>) {
        self.metadata.spec_version = Some(spec_version.into());
    }

    /// Record the harness that is driving this run (e.g. "temperpaw").
    pub fn set_harness(&mut self, harness: impl Into<String>) {
        self.metadata.harness = Some(harness.into());
    }

    /// Attach serving-stack token IDs to the current turn.
    ///
    /// Panics if no turn is in progress, or if a completion-aligned signal is
    /// already set at a different length — see [`OTSTurn::response_mask`].
    pub fn set_turn_token_ids(
        &mut self,
        prompt_token_ids: Vec<u32>,
        completion_token_ids: Vec<u32>,
    ) {
        let turn = self
            .current_turn
            .take()
            .expect("Cannot set token ids: no turn in progress");
        self.current_turn = Some(
            turn.with_prompt_token_ids(prompt_token_ids)
                .with_completion_token_ids(completion_token_ids),
        );
    }

    /// Attach the per-token response mask to the current turn.
    ///
    /// `1` marks a model-generated token, `0` a tool or otherwise injected
    /// token, one entry per completion token. Panics if no turn is in
    /// progress, if an entry is outside `{0, 1}`, or if a completion-aligned
    /// signal is already set at a different length.
    pub fn set_turn_response_mask(&mut self, response_mask: Vec<u8>) {
        let turn = self
            .current_turn
            .take()
            .expect("Cannot set response mask: no turn in progress");
        self.current_turn = Some(turn.with_response_mask(response_mask));
    }

    /// Attach per-token log probabilities to the current turn.
    ///
    /// One entry per completion token. Panics if no turn is in progress, or
    /// if a completion-aligned signal is already set at a different length.
    pub fn set_turn_logprobs(&mut self, logprobs: Vec<f64>) {
        let turn = self
            .current_turn
            .take()
            .expect("Cannot set logprobs: no turn in progress");
        self.current_turn = Some(turn.with_logprobs(logprobs));
    }

    /// Build a snapshot of the current trajectory without consuming the builder.
    ///
    /// Useful for mid-session uploads where the session should continue
    /// recording new turns after the upload.
    ///
    /// The in-progress turn goes through the same token-signal check
    /// [`Self::end_turn`] applies. A mid-turn flush is the one path that
    /// carries an unsealed turn into a document, and a turn whose
    /// completion-side signals do not yet agree is a malformed training
    /// sample: the upload endpoint refuses the whole document, so producing
    /// one here would lose the snapshot at the far end for a fault that is
    /// visible right here.
    pub fn snapshot(&self) -> OTSTrajectory {
        let mut metadata = self.metadata.clone();
        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        metadata.timestamp_end = Some(now);
        metadata.duration_ms = Some((now - metadata.timestamp_start).num_milliseconds() as f64);

        let mut turns = self.turns.clone();
        if let Some(ref current) = self.current_turn {
            if let Err(error) = current.validate_token_signals() {
                panic!("Cannot snapshot trajectory: {error}");
            }
            turns.push(current.clone());
        }

        let mut trajectory = OTSTrajectory::new(metadata);
        trajectory.context = self.context.clone();
        trajectory.system_message = self.system_message.clone();
        trajectory.turns = turns;
        trajectory
    }

    /// Build the final trajectory, consuming the builder.
    ///
    /// If a turn is still in progress, it is automatically ended using
    /// `sim_now()` as the end time.
    ///
    /// The metadata's `timestamp_end` is set to `sim_now()` and `duration_ms`
    /// is computed from the start/end timestamps.
    pub fn build(mut self) -> OTSTrajectory {
        // Auto-close any in-progress turn
        if self.current_turn.is_some() {
            let now = sim_now(); // determinism-ok: sim_now is DST-safe
            self.end_turn(now);
        }

        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        self.metadata.timestamp_end = Some(now);
        self.metadata.duration_ms =
            Some((now - self.metadata.timestamp_start).num_milliseconds() as f64);

        let mut trajectory = OTSTrajectory::new(self.metadata);
        trajectory.context = self.context;
        trajectory.system_message = self.system_message;
        trajectory.turns = self.turns;
        trajectory
    }
}

#[cfg(test)]
#[path = "builder_test.rs"]
mod builder_test;
