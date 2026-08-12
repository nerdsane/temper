//! Deterministic conformance checking of a recorded run against its spec.
//!
//! A trajectory says what an agent did. The IOA spec says what it was allowed
//! to do. [`check_conformance`] compares the two and reports every place they
//! disagree, with no sampling, no scoring, and no model in the loop: the same
//! inputs always produce the same report.
//!
//! # Inputs
//!
//! - **The automaton** — the actor spec the run executed under, as the kernel
//!   already parsed it. The checker reads the declared action set, each
//!   action's legal source states, and the terminal states.
//! - **The kernel rows** — one session's trajectory rows, oldest first, in the
//!   order the kernel wrote them. `query_trajectories_by_session` returns them
//!   in exactly that order; passing them in any other order gives a wrong
//!   answer, because the state-machine checks are order-dependent.
//! - **The OTS trajectory** (optional) — the agent-side record of the same
//!   run. It contributes the decisions the kernel never recorded a row for:
//!   actions the agent attempted that never reached the governed path.
//!
//! # The row stream and violation indices
//!
//! [`Violation::index`] is the position in the ordered stream the checker
//! walked. Positions `0..kernel_rows.len()` are kernel rows and index directly
//! into the slice that was passed in. Positions after that are OTS decisions,
//! appended in trajectory order.
//!
//! # What is checked, and what is skipped
//!
//! Only rows that belong to this actor are checked:
//!
//! - Rows whose `entity_type` is not this automaton's entity belong to another
//!   actor, whose spec the checker was not given. They are counted in
//!   [`ConformanceStats::other_entity_rows_skipped`] and not judged.
//! - Rows whose `source` is `Platform` are kernel bookkeeping — an entity-set
//!   miss, a spec submission, a progress marker — not actions the actor took.
//!   They are counted in [`ConformanceStats::platform_rows_skipped`].
//! - Rows marked `spec_governed = false` are caller-supplied audit records
//!   rather than governed dispatches. They are counted in
//!   [`ConformanceStats::non_governed_rows_skipped`].
//!
//! An OTS decision is checked only when it names an invocation
//! ([`temper_ots::models::DecisionType::is_invocation`]) and no kernel row **for this actor**
//! carries that action name. A decision the kernel did record is already
//! covered by its row, and checking both would report the same fault twice —
//! but a row belonging to another entity proves nothing about this one, so it
//! does not suppress the decision either. Decisions carry no observed state,
//! so only the action-set checks apply to them.
//!
//! # Verdict, and why `passed` is not just "no violations"
//!
//! A report that saw nothing found no violations, and so did a report that
//! saw the first 5,000 rows of a 5,001-row run. Neither is evidence of
//! conformance. [`ConformanceReport::verdict`] separates the three answers:
//! [`Verdict::Fail`] when the run disagreed with the spec, [`Verdict::Pass`]
//! when a complete run agreed with it, and [`Verdict::Indeterminate`] when the
//! evidence could not settle the question. [`ConformanceReport::evidence_gaps`]
//! names every reason. `passed` is true only for [`Verdict::Pass`], so a
//! consumer that gates on it cannot accept an unchecked run.
//!
//! # Violation kinds
//!
//! - [`ViolationKind::UnknownAction`] — the action name appears in no
//!   `[[action]]` of the spec and is not a name the kernel itself emits. No
//!   spec defines it anywhere.
//! - [`ViolationKind::ForbiddenAction`] — the action is one the platform
//!   defines (see [`KERNEL_PLATFORM_ACTIONS`]) but this actor's spec does not
//!   declare, recorded against the actor from the entity dispatch path. The
//!   name is defined; it is just not part of this actor's surface.
//! - [`ViolationKind::PostTerminal`] — an action on an entity that had already
//!   reached a terminal state. Terminal states are those no action lists in
//!   its `from`, so nothing may follow them. Detected both from an earlier row
//!   in the stream that drove the entity into a terminal state and from a row
//!   whose own `from_status` is terminal, so a session that begins after the
//!   terminal transition is still caught.
//! - [`ViolationKind::IllegalTransition`] — the row's `from_status` is not a
//!   legal source state for that action. The legal set is the action's `from`
//!   list, or, for an action declared without one, the values of its
//!   `state_in` guard. An `input` or `Composite` action with neither is always
//!   enabled — the I/O-automata property the kernel implements by giving it
//!   every state as a source — so nothing about its source state can disagree.
//! - [`ViolationKind::StateDiscontinuity`] — the row's `from_status` is not
//!   where the previous row for the same entity left it. Each row's source
//!   state can be legal on its own while the sequence skips a state no
//!   recorded action reached.
//! - [`ViolationKind::UnexpectedTargetState`] — a successful row landed
//!   somewhere other than the action's declared `to` (or, for an action with
//!   no `to`, somewhere other than where it started).
//! - [`ViolationKind::DeniedThenRetried`] — the same action was re-attempted
//!   on the same entity after an authorization denial with nothing having
//!   changed in between. A retry is justified when the entity's state changed
//!   between denial and retry (the precondition the denial evaluated no longer
//!   holds), or when the retry itself succeeded (authorization allowed it, so
//!   an approval landed). A retry that is refused again after neither is the
//!   agent hammering a closed door.
//!
//! Rows are judged independently, so one row can raise more than one
//! violation; `post_terminal` suppresses `illegal_transition` on the same row,
//! because a terminal source state is illegal for every action and reporting
//! it twice says nothing new.

mod walk;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use temper_ots::models::OTSTrajectory;
use temper_spec::automaton::Automaton;
use temper_store_turso::TursoTrajectoryRow;

use walk::{RowDisposition, SpecView, Walk, check_row, row_disposition, undeclared_detail};

/// Action names the kernel itself writes into the trajectory stream.
///
/// These are defined by the platform rather than by any actor spec: OData
/// write verbs, entity lifecycle markers, and management operations. A row
/// carrying one of these names against an actor that does not declare it is a
/// [`ViolationKind::ForbiddenAction`] — the name is defined, just not by that
/// actor — as opposed to a name no spec defines at all.
pub const KERNEL_PLATFORM_ACTIONS: &[&str] = &[
    "ContextReady",
    "Create",
    "Created",
    "Delete",
    "EntitySetNotFound",
    "Patch",
    "ProgressMade",
    "Put",
    "StreamUpdated",
    "SubmitSpec",
    "__Created",
    "manage_wasm",
    "submit_specs",
];

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
    /// Every disagreement, in stream order.
    pub violations: Vec<Violation>,
    /// Every reason the evidence was incomplete, in the order found. Empty
    /// when the checker saw the whole run.
    pub evidence_gaps: Vec<String>,
    /// What the checker looked at to get there.
    pub stats: ConformanceStats,
}

/// One run, and everything the checker needs to judge it.
pub struct ConformanceInput<'a> {
    /// The actor spec the run executed under.
    pub automaton: &'a Automaton,
    /// One session's rows, in the order the kernel wrote them, oldest first.
    pub kernel_rows: &'a [TursoTrajectoryRow],
    /// The agent-side record of the same run, when one was supplied.
    pub ots_trajectory: Option<&'a OTSTrajectory>,
    /// Whether the row read stopped at its cap instead of at the end of the
    /// session. A checked prefix is not a checked run, so this makes the
    /// verdict indeterminate rather than passing.
    pub rows_truncated: bool,
}

/// Check one recorded run against the spec that governed it.
///
/// See the module docs for what each violation kind means, which rows are
/// judged, and how the verdict follows from the violations and the evidence.
pub fn check_conformance(input: ConformanceInput<'_>) -> ConformanceReport {
    let ConformanceInput {
        automaton,
        kernel_rows,
        ots_trajectory,
        rows_truncated,
    } = input;
    let spec = SpecView::new(automaton);
    let mut walk = Walk::default();
    let mut violations: Vec<Violation> = Vec::new();

    for (index, row) in kernel_rows.iter().enumerate() {
        check_row(&spec, &mut walk, index, row, &mut violations);
    }

    let mut stats = walk.into_stats(kernel_rows.len());

    if let Some(trajectory) = ots_trajectory {
        // Only this actor's rows can account for a decision. A `PayInvoice`
        // row on `Invoice` says nothing about whether the agent's `PayInvoice`
        // decision against `Order` ever reached the governed path, so it must
        // not suppress it.
        let recorded_actions: BTreeSet<&str> = kernel_rows
            .iter()
            .filter(|row| row_disposition(&spec, row) == RowDisposition::ActorExecution)
            .map(|row| row.action.as_str())
            .collect();
        let mut index = kernel_rows.len();
        for decision in trajectory.turns.iter().flat_map(|turn| &turn.decisions) {
            // A reasoning step or a response formulation names a thought, not
            // a callable; reporting it as an action invents an attempt the
            // agent never made.
            if !decision.decision_type.is_invocation() {
                stats.ots_decisions_skipped_as_thinking += 1;
                continue;
            }
            let action = decision.choice.action.as_str();
            if recorded_actions.contains(action) {
                continue;
            }
            stats.ots_decisions_checked += 1;
            if !spec.declared.contains_key(action) {
                let kind = spec.classify_undeclared(action);
                violations.push(Violation {
                    index,
                    kind,
                    action: action.to_string(),
                    entity_type: spec.entity_name.to_string(),
                    detail: format!(
                        "agent decided on `{action}`, which the kernel never recorded a row for; \
                         {}",
                        undeclared_detail(kind, spec.entity_name)
                    ),
                });
            }
            index += 1;
        }
        stats.stream_length = index;
    }

    for violation in &violations {
        *stats
            .violations_by_kind
            .entry(violation.kind.as_str().to_string())
            .or_insert(0) += 1;
    }

    let evidence_gaps = evidence_gaps(&stats, kernel_rows.len(), rows_truncated);
    let verdict = if !violations.is_empty() {
        // A disagreement found in a prefix is still a disagreement.
        Verdict::Fail
    } else if evidence_gaps.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Indeterminate
    };

    ConformanceReport {
        verdict,
        passed: verdict == Verdict::Pass,
        violations,
        evidence_gaps,
        stats,
    }
}

/// Every reason this report is not evidence of a conforming run.
fn evidence_gaps(
    stats: &ConformanceStats,
    kernel_rows_read: usize,
    rows_truncated: bool,
) -> Vec<String> {
    let mut gaps = Vec::new();
    if rows_truncated {
        gaps.push(format!(
            "the read stopped at its row cap after {kernel_rows_read} rows, so the rest of the \
             session was never checked"
        ));
    }
    if stats.actor_rows == 0 && stats.ots_decisions_checked == 0 {
        gaps.push(
            "no rows and no decisions were judged, so nothing about this run was checked"
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

#[cfg(test)]
mod conformance_test;
