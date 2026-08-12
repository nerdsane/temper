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
//! - **The spec resolution** — whether the run named the spec version it
//!   executed under. See [`SpecResolution`].
//!
//! # The row stream and violation indices
//!
//! [`Violation::index`] is the position in the ordered stream the checker
//! walked. Positions `0..kernel_rows.len()` are kernel rows and index directly
//! into the slice that was passed in. Positions after that are the actions
//! named by OTS decisions, appended in trajectory order.
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
//! - Capture-loss markers ([`CAPTURE_LOSS_ENTITY_TYPE`]) are not actions at
//!   all. They are the capture path saying it failed to store a row for this
//!   session, and they are counted in
//!   [`ConformanceStats::capture_loss_markers`].
//!
//! An OTS decision is checked only when it names a governed action (see
//! [`decisions`]) and no kernel row **for this actor** carries that action
//! name. A decision the kernel did record is already covered by its row, and
//! checking both would report the same fault twice — but a row belonging to
//! another entity proves nothing about this one, so it does not suppress the
//! decision either. Decisions carry no observed state, so only the action-set
//! checks apply to them.
//!
//! # Verdict, and why `passed` is not just "no violations"
//!
//! A report that saw nothing found no violations, and so did a report that
//! saw the first 5,000 rows of a 5,001-row run. Neither is evidence of
//! conformance. [`ConformanceReport::verdict`] separates the three answers:
//! [`Verdict::Fail`] when the run disagreed with the spec, [`Verdict::Pass`]
//! when a complete run agreed with it, and [`Verdict::Indeterminate`] when the
//! evidence could not settle the question. [`ConformanceReport::evidence_gaps`]
//! names every reason and [`ConformanceReport::evidence_complete`] is the
//! one-field form of the same answer. `passed` is true only for
//! [`Verdict::Pass`], so a consumer that gates on it cannot accept an unchecked
//! run.
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

mod decisions;
mod report;
mod spec_view;
mod walk;

use std::collections::BTreeSet;

use temper_ots::models::OTSTrajectory;
use temper_spec::automaton::Automaton;
use temper_store_turso::TursoTrajectoryRow;

use decisions::{DecisionActions, decision_actions};
use report::{EvidenceContext, evidence_gaps};
use spec_view::SpecView;
use walk::{RowDisposition, Walk, check_row, row_disposition, undeclared_detail};

pub use report::{
    ConformanceReport, ConformanceStats, SpecResolution, Verdict, Violation, ViolationKind,
};

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

/// Entity type of the row the capture path writes when it loses a trajectory
/// entry for a session.
///
/// The trajectory outbox is bounded and its writes can fail. Either way an
/// action the kernel captured never reaches storage, and a session read that
/// silently returns the rows that survived would let a run with holes in it
/// pass a conformance check. The capture path writes one marker per session it
/// loses a row for (`crate::trajectory_outbox`); this checker reads the marker
/// as an evidence gap.
///
/// Not an actor's entity type and not an action: markers are never judged.
pub const CAPTURE_LOSS_ENTITY_TYPE: &str = "TrajectoryCapture";

/// Action name on a capture-loss marker row.
pub const CAPTURE_LOSS_ACTION: &str = "CaptureLost";

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
    /// Whether `automaton` is provably the spec that governed the run.
    pub spec_resolution: SpecResolution,
    /// Whether the server holding these rows has lost captured rows it could
    /// not record against any session. When true, no session read from it can
    /// be assumed whole, so the report says so and cannot pass.
    pub capture_degraded: bool,
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
        spec_resolution,
        capture_degraded,
    } = input;
    let spec = SpecView::new(automaton);
    let mut walk = Walk::default();
    let mut violations: Vec<Violation> = Vec::new();

    for (index, row) in kernel_rows.iter().enumerate() {
        check_row(&spec, &mut walk, index, row, &mut violations);
    }

    let mut stats = walk.into_stats(kernel_rows.len());

    if let Some(trajectory) = ots_trajectory {
        check_decisions(&spec, trajectory, kernel_rows, &mut stats, &mut violations);
    }

    for violation in &violations {
        *stats
            .violations_by_kind
            .entry(violation.kind.as_str().to_string())
            .or_insert(0) += 1;
    }

    let gaps = evidence_gaps(
        &stats,
        &EvidenceContext {
            kernel_rows_read: kernel_rows.len(),
            rows_truncated,
            spec_resolution,
            capture_degraded,
        },
    );
    ConformanceReport::new(violations, gaps, spec_resolution, stats)
}

/// Judge the actions the agent decided on that no kernel row accounts for.
///
/// Decisions are a one-way input. They can raise violations, and so fail a run;
/// they never count toward passing one. A decision is the agent's own account
/// of what it chose — it names an action and carries no observed state, so it
/// cannot show that anything happened, let alone that it happened legally.
/// [`ConformanceStats::ots_decisions_checked`] is a record of what was looked
/// at, not evidence, which is why the "nothing was checked" gap in
/// [`report`] keys on kernel rows alone.
fn check_decisions(
    spec: &SpecView<'_>,
    trajectory: &OTSTrajectory,
    kernel_rows: &[TursoTrajectoryRow],
    stats: &mut ConformanceStats,
    violations: &mut Vec<Violation>,
) {
    // Only this actor's rows can account for a decision. A `PayInvoice` row on
    // `Invoice` says nothing about whether the agent's `PayInvoice` decision
    // against `Order` ever reached the governed path, so it must not suppress
    // it.
    let recorded_actions: BTreeSet<&str> = kernel_rows
        .iter()
        .filter(|row| row_disposition(spec, row) == RowDisposition::ActorExecution)
        .map(|row| row.action.as_str())
        .collect();
    let mut index = kernel_rows.len();
    for decision in trajectory.turns.iter().flat_map(|turn| &turn.decisions) {
        let actions = match decision_actions(decision) {
            // A reasoning step or a response formulation names a thought, not
            // a callable; reporting it as an action invents an attempt the
            // agent never made.
            DecisionActions::Thinking => {
                stats.ots_decisions_skipped_as_thinking += 1;
                continue;
            }
            // The agent invoked its harness, not this actor. The governed
            // actions it reached through that call, if any, are recorded
            // alongside it and are judged as `Actions` instead.
            DecisionActions::HarnessTool => {
                stats.ots_decisions_skipped_as_harness_tool += 1;
                continue;
            }
            DecisionActions::Actions(actions) => actions,
        };
        for action in actions {
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
    }
    stats.stream_length = index;
}

#[cfg(test)]
mod conformance_test;
