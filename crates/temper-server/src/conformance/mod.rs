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
//!
//! An OTS decision is checked only when no kernel row in the session carries
//! that action name. A decision the kernel did record is already covered by
//! its row, and checking both would report the same fault twice. Decisions
//! carry no observed state, so only the action-set checks apply to them.
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
//!   `state_in` guard. An action with neither has no state precondition and is
//!   not checked.
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

use walk::{SpecView, Walk, check_row, undeclared_detail};

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
            Self::ForbiddenAction => "forbidden_action",
            Self::PostTerminal => "post_terminal",
            Self::DeniedThenRetried => "denied_then_retried",
            Self::UnknownAction => "unknown_action",
        }
    }
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
    /// Judged rows whose `from_status` was absent, so no transition could be
    /// checked. A high count means the capture path, not the run, is the
    /// thing to look at.
    pub transitions_unchecked: usize,
    /// OTS decisions judged because no kernel row carried their action.
    pub ots_decisions_checked: usize,
    /// Entities observed reaching a terminal state.
    pub terminal_entities: usize,
    /// Violation count per kind, keyed by [`ViolationKind::as_str`].
    pub violations_by_kind: BTreeMap<String, usize>,
}

/// The result of checking one run against one spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// True when no violation was found.
    pub passed: bool,
    /// Every disagreement, in stream order.
    pub violations: Vec<Violation>,
    /// What the checker looked at to get there.
    pub stats: ConformanceStats,
}

/// Check one recorded run against the spec that governed it.
///
/// `kernel_rows` must be in the order the kernel wrote them, oldest first.
/// `ots_trajectory` is optional; when present it contributes the agent-side
/// decisions the kernel never recorded a row for.
///
/// See the module docs for what each violation kind means and which rows are
/// judged.
pub fn check_conformance(
    automaton: &Automaton,
    kernel_rows: &[TursoTrajectoryRow],
    ots_trajectory: Option<&OTSTrajectory>,
) -> ConformanceReport {
    let spec = SpecView::new(automaton);
    let mut walk = Walk::default();
    let mut violations: Vec<Violation> = Vec::new();

    for (index, row) in kernel_rows.iter().enumerate() {
        check_row(&spec, &mut walk, index, row, &mut violations);
    }

    let mut stats = walk.into_stats(kernel_rows.len());

    if let Some(trajectory) = ots_trajectory {
        let recorded_actions: BTreeSet<&str> =
            kernel_rows.iter().map(|row| row.action.as_str()).collect();
        let mut index = kernel_rows.len();
        for decision in trajectory.turns.iter().flat_map(|turn| &turn.decisions) {
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

    ConformanceReport {
        passed: violations.is_empty(),
        violations,
        stats,
    }
}

#[cfg(test)]
mod conformance_test;
