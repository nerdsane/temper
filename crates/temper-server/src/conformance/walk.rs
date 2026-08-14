//! The ordered walk over one session's rows.
//!
//! [`check_conformance`](super::check_conformance) owns the entry point and
//! the report shape; this module owns the per-row judgements and the state
//! carried between them.

use std::collections::BTreeMap;

use temper_spec::automaton::Action;
use temper_store_turso::TursoTrajectoryRow;

use super::spec_view::{SourceStates, SpecView};
use super::{CAPTURE_LOSS_ACTION, CAPTURE_LOSS_ENTITY_TYPE};
use super::{ConformanceStats, Violation, ViolationKind};

/// Whether a row records something this actor's spec governs.
///
/// Four kinds of row reach the checker and only one of them is the actor
/// executing its spec:
///
/// - a capture-loss marker, which is the capture path reporting that a row for
///   this session never reached storage;
/// - another actor's row, whose spec the checker was not given;
/// - kernel bookkeeping (`source = Platform`);
/// - a row explicitly marked `spec_governed = false`: a report about a run
///   rather than a record of one. The caller-supplied endpoints (`POST
///   /api/audit`, `POST /api/evolution/trajectories/unmet`, the load-inline
///   SubmitSpec note) and the management-plane / pre-flight authorization
///   denials (`POST /api/authorize`, policy auth, WASM and spec management)
///   all write these — their session, entity type, or action can be
///   caller-chosen, so judging them would let any caller inject violations
///   into another session's report.
///
/// A row with `spec_governed` absent is a governed dispatch: the kernel's
/// entity-dispatch capture sites (including the OData dispatch-guard denials)
/// leave the column unset, and every caller-influenced writer sets it false.
pub(super) fn row_disposition(spec: &SpecView<'_>, row: &TursoTrajectoryRow) -> RowDisposition {
    // Checked before the entity comparison: a marker's entity type is the
    // capture path's own, so the `OtherEntity` arm would otherwise swallow it
    // and the evidence gap it reports would be lost.
    if row.entity_type == CAPTURE_LOSS_ENTITY_TYPE && row.action == CAPTURE_LOSS_ACTION {
        return RowDisposition::CaptureLoss;
    }
    if row.entity_type != spec.entity_name {
        return RowDisposition::OtherEntity;
    }
    if row.source.as_deref() == Some("Platform") {
        return RowDisposition::PlatformBookkeeping;
    }
    if row.spec_governed == Some(false) {
        return RowDisposition::NotSpecGoverned;
    }
    RowDisposition::ActorExecution
}

/// What a row is, for the checker's purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowDisposition {
    /// This actor executing its spec: judged.
    ActorExecution,
    /// The capture path reporting a row it failed to store.
    CaptureLoss,
    /// Another actor's row.
    OtherEntity,
    /// Kernel bookkeeping.
    PlatformBookkeeping,
    /// A caller-supplied audit record, not a governed dispatch.
    NotSpecGoverned,
}

pub(super) fn check_row(
    spec: &SpecView<'_>,
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    violations: &mut Vec<Violation>,
) {
    match row_disposition(spec, row) {
        RowDisposition::CaptureLoss => {
            walk.capture_loss_markers += 1;
            return;
        }
        RowDisposition::OtherEntity => {
            walk.other_entity_rows_skipped += 1;
            return;
        }
        RowDisposition::PlatformBookkeeping => {
            walk.platform_rows_skipped += 1;
            return;
        }
        RowDisposition::NotSpecGoverned => {
            walk.non_governed_rows_skipped += 1;
            return;
        }
        RowDisposition::ActorExecution => {}
    }
    walk.actor_rows += 1;

    let action_name = row.action.as_str();
    let Some(action) = spec.declared.get(action_name) else {
        let kind = spec.classify_undeclared(action_name);
        violations.push(Violation {
            index,
            kind,
            action: action_name.to_string(),
            entity_type: row.entity_type.clone(),
            detail: undeclared_detail(kind, spec.entity_name),
        });
        // The row is not judged further — there is no declared action to judge
        // it against — but it still moved the entity, and the walk has to know
        // where to. Leaving the state behind would make the next legal row
        // look like a discontinuity, reporting one fault twice.
        record_state_progress(spec, walk, index, row);
        return;
    };

    let entity_key = row.entity_id.as_str();
    let post_terminal = match walk.terminal_at.get(entity_key) {
        Some(terminal_index) => Some(format!(
            "entity reached terminal state at index {terminal_index}; no action may follow"
        )),
        None => row
            .from_status
            .as_deref()
            .filter(|status| spec.terminal_states.contains(*status))
            .map(|status| {
                format!("acted from terminal state `{status}`, which no action lists as a source")
            }),
    };

    if let Some(detail) = post_terminal {
        violations.push(Violation {
            index,
            kind: ViolationKind::PostTerminal,
            action: action_name.to_string(),
            entity_type: row.entity_type.clone(),
            detail,
        });
    } else {
        check_transition(spec, walk, index, row, action, violations);
    }

    check_continuity(walk, index, row, violations);
    check_target_state(spec, walk, index, row, action, violations);
    check_retry(walk, index, row, violations);
    record_state_progress(spec, walk, index, row);
}

fn check_transition(
    spec: &SpecView<'_>,
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    action: &Action,
    violations: &mut Vec<Violation>,
) {
    let Some(from_status) = row.from_status.as_deref() else {
        walk.transitions_unchecked += 1;
        return;
    };
    let legal_sources = match spec.legal_sources(action) {
        SourceStates::Declared(states) => states,
        // Legal from every state, so nothing to disagree with.
        SourceStates::AnyState => return,
        SourceStates::Unevaluable => {
            // The spec declares no source set this checker can evaluate, so
            // the run's source state was not checked. Counted rather than
            // passed over, because an unchecked transition is missing
            // evidence and the report says so.
            walk.transitions_unchecked += 1;
            return;
        }
    };
    if legal_sources.contains(from_status) {
        return;
    }
    let legal = legal_sources.iter().copied().collect::<Vec<_>>().join(", ");
    violations.push(Violation {
        index,
        kind: ViolationKind::IllegalTransition,
        action: row.action.clone(),
        entity_type: row.entity_type.clone(),
        detail: format!(
            "acted from `{from_status}`, but `{}` is legal only from: {legal}",
            row.action
        ),
    });
}

/// Check that the row starts where the previous row for the same entity left
/// off.
///
/// Judging each row's `from_status` on its own accepts an impossible run:
/// `Submit: Draft -> Submitted` followed by `Ship: Processing -> Shipped` has
/// two individually legal source states and a gap between them that no
/// recorded action crossed. The state machine is a walk, so the checker walks
/// it.
fn check_continuity(
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    violations: &mut Vec<Violation>,
) {
    let (Some(from_status), Some((previous_index, previous_status))) = (
        row.from_status.as_deref(),
        walk.last_observed_status.get(row.entity_id.as_str()),
    ) else {
        return;
    };
    if from_status == previous_status {
        return;
    }
    violations.push(Violation {
        index,
        kind: ViolationKind::StateDiscontinuity,
        action: row.action.clone(),
        entity_type: row.entity_type.clone(),
        detail: format!(
            "acted from `{from_status}`, but the entity was last recorded in `{previous_status}` \
             at index {previous_index}; no recorded action crossed the gap"
        ),
    });
}

/// Check that a successful row landed where the spec says the action lands.
///
/// A stored row claims both ends of the transition. Reading only the source
/// state accepts `Submit: Draft -> Cancelled` against a spec whose `Submit`
/// goes to `Submitted`.
fn check_target_state(
    spec: &SpecView<'_>,
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    action: &Action,
    violations: &mut Vec<Violation>,
) {
    if !row.success {
        // A failed action changes nothing, so there is no target to check.
        return;
    }
    if spec.is_emitted_event(action) {
        // An output action is emitted to the environment; it carries no
        // transition, so it has no target state to agree with.
        return;
    }
    let Some(to_status) = row.to_status.as_deref() else {
        walk.targets_unchecked += 1;
        return;
    };
    // An action without a `to` leaves the state where it was; that is the
    // target it must land on.
    let (expected, source) = match action.to.as_deref() {
        Some(expected) => (expected, "the action's `to`"),
        None => match row.from_status.as_deref() {
            Some(from_status) => (
                from_status,
                "the action declares no `to`, so the state holds",
            ),
            None => {
                walk.targets_unchecked += 1;
                return;
            }
        },
    };
    if to_status == expected {
        return;
    }
    violations.push(Violation {
        index,
        kind: ViolationKind::UnexpectedTargetState,
        action: row.action.clone(),
        entity_type: row.entity_type.clone(),
        detail: format!(
            "succeeded into `{to_status}`, but `{}` lands in `{expected}` ({source})",
            row.action
        ),
    });
}

fn check_retry(
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    violations: &mut Vec<Violation>,
) {
    let key = (row.entity_id.clone(), row.action.clone());
    if let Some(&denial_index) = walk.pending_denial.get(&key) {
        let state_changed = walk
            .last_state_change
            .get(row.entity_id.as_str())
            .is_some_and(|change_index| *change_index > denial_index);
        if !row.success && !state_changed {
            violations.push(Violation {
                index,
                kind: ViolationKind::DeniedThenRetried,
                action: row.action.clone(),
                entity_type: row.entity_type.clone(),
                detail: format!(
                    "retried `{}` after the denial at index {denial_index} with no intervening \
                     approval or state change",
                    row.action
                ),
            });
        }
    }

    if row.authz_denied == Some(true) {
        walk.pending_denial.insert(key, index);
    } else if row.success {
        // The action went through, so whatever blocked it no longer does.
        walk.pending_denial.remove(&key);
    }
}

fn record_state_progress(
    spec: &SpecView<'_>,
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
) {
    // A row that reports where the entity ended up moves the walk's idea of
    // where the entity is, whether or not the action succeeded: a failed
    // action reports the unchanged state, and that is still the state the next
    // row must start from. A denial reports no state at all and leaves the
    // last observation standing.
    if let Some(to_status) = row.to_status.as_deref() {
        walk.last_observed_status
            .insert(row.entity_id.clone(), (index, to_status.to_string()));
    }

    if !row.success {
        return;
    }
    let Some(to_status) = row.to_status.as_deref() else {
        return;
    };
    if Some(to_status) != row.from_status.as_deref() {
        walk.last_state_change.insert(row.entity_id.clone(), index);
    }
    if spec.terminal_states.contains(to_status) {
        walk.terminal_at
            .entry(row.entity_id.clone())
            .or_insert(index);
    }
}

pub(super) fn undeclared_detail(kind: ViolationKind, entity_name: &str) -> String {
    match kind {
        ViolationKind::ForbiddenAction => format!(
            "the platform defines this action, but the `{entity_name}` spec does not declare it"
        ),
        _ => format!("no `[[action]]` in the `{entity_name}` spec defines this name"),
    }
}

/// State carried across the row stream.
#[derive(Default)]
pub(super) struct Walk {
    actor_rows: usize,
    platform_rows_skipped: usize,
    other_entity_rows_skipped: usize,
    non_governed_rows_skipped: usize,
    capture_loss_markers: usize,
    transitions_unchecked: usize,
    targets_unchecked: usize,
    /// Index at which each entity first reached a terminal state.
    terminal_at: BTreeMap<String, usize>,
    /// Index of each entity's most recent successful status change.
    last_state_change: BTreeMap<String, usize>,
    /// Index and status of each entity's most recent reported end state.
    last_observed_status: BTreeMap<String, (usize, String)>,
    /// Index of the most recent unresolved denial per (entity, action).
    pending_denial: BTreeMap<(String, String), usize>,
}

impl Walk {
    pub(super) fn into_stats(self, stream_length: usize) -> ConformanceStats {
        ConformanceStats {
            stream_length,
            actor_rows: self.actor_rows,
            platform_rows_skipped: self.platform_rows_skipped,
            other_entity_rows_skipped: self.other_entity_rows_skipped,
            non_governed_rows_skipped: self.non_governed_rows_skipped,
            capture_loss_markers: self.capture_loss_markers,
            transitions_unchecked: self.transitions_unchecked,
            targets_unchecked: self.targets_unchecked,
            ots_decisions_checked: 0,
            ots_decisions_skipped_as_thinking: 0,
            ots_decisions_skipped_as_harness_tool: 0,
            terminal_entities: self.terminal_at.len(),
            violations_by_kind: BTreeMap::new(),
        }
    }
}
