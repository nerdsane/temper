//! What a conformance check answers with.
//!
//! [`check_conformance`](super::check_conformance) owns the walk; this module
//! owns the shape of the answer and the rule that turns violations and evidence
//! into a verdict.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The kind of disagreement between a recorded run and its spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// The action's source state is not legal for that action.
    IllegalTransition,
    /// The action's source state is not where the previous row left the entity.
    StateDiscontinuity,
    /// A successful action landed somewhere the spec does not send it.
    UnexpectedTargetState,
    /// The action is defined by the platform but not by this actor's spec.
    ForbiddenAction,
    /// The action followed a terminal state.
    PostTerminal,
    /// The action was retried after a denial with nothing changed in between.
    DeniedThenRetried,
    /// No spec defines the action name.
    UnknownAction,
}

impl ViolationKind {
    /// Stable snake_case name, used as the key in
    /// [`ConformanceStats::violations_by_kind`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IllegalTransition => "illegal_transition",
            Self::StateDiscontinuity => "state_discontinuity",
            Self::UnexpectedTargetState => "unexpected_target_state",
            Self::ForbiddenAction => "forbidden_action",
            Self::PostTerminal => "post_terminal",
            Self::DeniedThenRetried => "denied_then_retried",
            Self::UnknownAction => "unknown_action",
        }
    }
}

/// What the checker was able to conclude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A complete run, and it agreed with its spec.
    Pass,
    /// The run disagreed with its spec.
    Fail,
    /// The evidence could not settle the question.
    Indeterminate,
}

/// Whether the spec the run was judged against is the spec that governed it.
///
/// A conformance report only means something against the spec the run executed
/// under. Temper keeps one spec per (tenant, entity type), so the registered
/// spec is whatever was submitted last — not necessarily the one in force when
/// the run happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecResolution {
    /// The run named the version it executed under, and it is the registered
    /// spec the check ran against.
    Pinned,
    /// Nothing named the governing version. The check ran against whatever is
    /// registered now, which may or may not be what the run executed under, so
    /// the report cannot claim conformance.
    Unresolved,
}

/// One place where the run disagreed with the spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// Position in the ordered row stream the checker walked.
    pub index: usize,
    /// What kind of disagreement this is.
    pub kind: ViolationKind,
    /// The action that raised it.
    pub action: String,
    /// The entity type the action was taken on.
    pub entity_type: String,
    /// Why it is a violation, naming the states or indices involved.
    pub detail: String,
}

/// What the checker looked at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceStats {
    /// Rows and decisions in the ordered stream.
    pub stream_length: usize,
    /// Kernel rows attributed to this actor and judged.
    pub actor_rows: usize,
    /// Kernel rows skipped as kernel bookkeeping (`source = Platform`).
    pub platform_rows_skipped: usize,
    /// Kernel rows skipped as belonging to another actor's spec.
    pub other_entity_rows_skipped: usize,
    /// Kernel rows skipped as caller-supplied audit records
    /// (`spec_governed = false`) rather than governed dispatches.
    pub non_governed_rows_skipped: usize,
    /// Capture-loss markers found in the session
    /// (see [`CAPTURE_LOSS_ENTITY_TYPE`](super::CAPTURE_LOSS_ENTITY_TYPE)).
    /// Each one says the capture path failed to store at least one row for
    /// this session, so the record the checker walked has holes in it.
    pub capture_loss_markers: usize,
    /// Judged rows whose source state could not be checked — `from_status`
    /// was absent, or the spec declares no source set to check it against. A
    /// high count means the capture path, not the run, is the thing to look
    /// at.
    pub transitions_unchecked: usize,
    /// Judged rows whose target state could not be checked, because the row
    /// reported no end state.
    pub targets_unchecked: usize,
    /// OTS decisions judged because no kernel row for this actor carried their
    /// action.
    pub ots_decisions_checked: usize,
    /// OTS decisions passed over because their type names a thought rather
    /// than a callable (see
    /// [`temper_ots::models::DecisionType::is_invocation`]).
    pub ots_decisions_skipped_as_thinking: usize,
    /// OTS decisions passed over because they name the harness's own tool
    /// rather than a governed action, and carry no governed action inside
    /// them (see [`decisions`](super::decisions)).
    pub ots_decisions_skipped_as_harness_tool: usize,
    /// OTS decisions passed over because their name is in no action vocabulary
    /// the kernel knows: not declared by this actor's spec, not defined by the
    /// platform, and not dispatched anywhere in this session. In a run driven
    /// by an agent harness this is the harness's own tooling — `Bash`, `Read`,
    /// an MCP tool — and it is normally the largest count in this struct.
    ///
    /// It is also the checker's blind spot, which is why it is counted rather
    /// than folded into the line above: an action name the kernel has never
    /// seen is indistinguishable from a tool name, so a decision claiming an
    /// action that never reached the platform lands here instead of in
    /// [`Self::violations_by_kind`]. Actions that *did* reach the platform are
    /// unaffected — they have rows, and rows are judged in full. See
    /// [`decisions`](super::decisions) for the rule and its limits.
    pub ots_decisions_skipped_as_unrecognized_name: usize,
    /// Entities observed reaching a terminal state.
    pub terminal_entities: usize,
    /// Violation count per kind, keyed by [`ViolationKind::as_str`].
    pub violations_by_kind: BTreeMap<String, usize>,
}

/// The result of checking one run against one spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// What the checker concluded.
    pub verdict: Verdict,
    /// True only for [`Verdict::Pass`]: a complete run that agreed with its
    /// spec. False for a failure and false for evidence that could not settle
    /// the question, so a consumer gating on this field cannot accept an
    /// unchecked run.
    pub passed: bool,
    /// Whether the spec the run was judged against is provably the one that
    /// governed it.
    pub spec_resolution: SpecResolution,
    /// True when the checker saw the whole run. False when any of the evidence
    /// it needed was missing — the read stopped at its cap, the session was
    /// empty, the capture path recorded a loss, or the governing spec could not
    /// be resolved. Always the negation of "[`evidence_gaps`] is non-empty",
    /// as a field a consumer can gate on without reading prose.
    pub evidence_complete: bool,
    /// Every disagreement, in stream order.
    pub violations: Vec<Violation>,
    /// Every reason the evidence was incomplete, in the order found. Empty
    /// when the checker saw the whole run.
    ///
    /// [`evidence_gaps`]: ConformanceReport::evidence_gaps
    pub evidence_gaps: Vec<String>,
    /// What the checker looked at to get there.
    pub stats: ConformanceStats,
}

impl ConformanceReport {
    /// Assemble the answer from the walk's findings.
    ///
    /// One place decides the verdict, so `passed` and `evidence_complete`
    /// cannot drift from the gaps that produced them.
    pub(super) fn new(
        violations: Vec<Violation>,
        evidence_gaps: Vec<String>,
        spec_resolution: SpecResolution,
        stats: ConformanceStats,
    ) -> Self {
        let evidence_complete = evidence_gaps.is_empty();
        let verdict = if !violations.is_empty() {
            // A disagreement found in a prefix is still a disagreement.
            Verdict::Fail
        } else if evidence_complete {
            Verdict::Pass
        } else {
            Verdict::Indeterminate
        };
        Self {
            verdict,
            passed: verdict == Verdict::Pass,
            spec_resolution,
            evidence_complete,
            violations,
            evidence_gaps,
            stats,
        }
    }
}

/// What the checker knows about its inputs, beyond the rows themselves.
pub(super) struct EvidenceContext {
    /// Kernel rows the read returned.
    pub kernel_rows_read: usize,
    /// Whether the read stopped at its cap rather than the end of the session.
    pub rows_truncated: bool,
    /// Whether the spec judged is provably the one that governed the run.
    pub spec_resolution: SpecResolution,
    /// Whether this server has lost captured rows it could not attribute to
    /// any session.
    pub capture_degraded: bool,
}

/// Every reason this report is not evidence of a conforming run.
pub(super) fn evidence_gaps(stats: &ConformanceStats, context: &EvidenceContext) -> Vec<String> {
    let EvidenceContext {
        kernel_rows_read,
        rows_truncated,
        spec_resolution,
        capture_degraded,
    } = *context;
    let mut gaps = Vec::new();
    if spec_resolution == SpecResolution::Unresolved {
        gaps.push(
            "neither the request nor the trajectory named the spec version this run executed \
             under, so it was checked against whatever is registered now; Temper keeps one spec \
             per entity type, and a submit replaces it, so that may not be the spec that governed \
             the run"
                .to_string(),
        );
    }
    if rows_truncated {
        gaps.push(format!(
            "the read stopped at its row cap after {kernel_rows_read} rows, so the rest of the \
             session was never checked"
        ));
    }
    if stats.capture_loss_markers > 0 {
        gaps.push(format!(
            "the capture path recorded {} loss marker(s) for this session, so at least one row it \
             captured was never stored and the run is missing from the record in unknown places",
            stats.capture_loss_markers
        ));
    }
    // A marker says which session lost a row. This says a row was lost and the
    // marker for it could not be written either, so no session's record can be
    // trusted to be whole — including this one, whether or not it has a marker.
    if capture_degraded {
        gaps.push(
            "this server has lost captured rows it could not record against any session, so any \
             session read from it may be missing rows with nothing to say so"
                .to_string(),
        );
    }
    // Keyed on the kernel's rows alone. An agent-side decision names an action
    // and carries no observed state, so it can contradict a spec but can never
    // show that a run followed one. Counting decisions here would let a
    // trajectory naming a single declared action stand in as the whole
    // evidence for a session the kernel recorded nothing about.
    if stats.actor_rows == 0 {
        gaps.push(
            "the kernel recorded no governed action for this actor, so nothing about what the \
             run did was checked; agent-side decisions can disagree with a spec but never \
             establish that a run followed one, because they carry no observed state"
                .to_string(),
        );
    }
    if stats.transitions_unchecked > 0 {
        gaps.push(format!(
            "{} row(s) carried no source state the spec could be checked against",
            stats.transitions_unchecked
        ));
    }
    if stats.targets_unchecked > 0 {
        gaps.push(format!(
            "{} successful row(s) reported no end state, so where they landed is unknown",
            stats.targets_unchecked
        ));
    }
    gaps
}
