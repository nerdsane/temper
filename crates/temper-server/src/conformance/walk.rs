//! The ordered walk over one session's rows.
//!
//! [`check_conformance`](super::check_conformance) owns the entry point and
//! the report shape; this module owns the per-row judgements and the state
//! carried between them.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, Automaton, Guard};
use temper_store_turso::TursoTrajectoryRow;

use super::{ConformanceStats, KERNEL_PLATFORM_ACTIONS, Violation, ViolationKind};

pub(super) fn check_row(
    spec: &SpecView<'_>,
    walk: &mut Walk,
    index: usize,
    row: &TursoTrajectoryRow,
    violations: &mut Vec<Violation>,
) {
    if row.entity_type != spec.entity_name {
        walk.other_entity_rows_skipped += 1;
        return;
    }
    if row.source.as_deref() == Some("Platform") {
        walk.platform_rows_skipped += 1;
        return;
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
    let Some(legal_sources) = spec.legal_sources(action) else {
        // No `from` list and no `state_in` guard: the action carries no state
        // precondition, so every source state is legal for it.
        return;
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

/// Legal source states for an action, or `None` when it declares no state
/// precondition at all.
fn legal_sources(action: &Action) -> Option<BTreeSet<&str>> {
    if !action.from.is_empty() {
        return Some(action.from.iter().map(String::as_str).collect());
    }
    // An action written with a `state_in` guard instead of a `from` list still
    // restricts its source states; read the guard rather than treating the
    // action as unconstrained.
    let guarded: BTreeSet<&str> = action
        .guard
        .iter()
        .filter_map(|guard| match guard {
            Guard::StateIn { values } => Some(values.iter().map(String::as_str)),
            _ => None,
        })
        .flatten()
        .collect();
    (!guarded.is_empty()).then_some(guarded)
}

/// States no action can fire from.
///
/// This is the kernel's own terminal-state rule — a state no action lists as a
/// source — extended to read `state_in` guards as well as `from` lists.
/// `Automaton::extract_metadata` reads only `from`, so an action written with
/// a guard instead would leave its own source states looking terminal and turn
/// every legal action out of them into a false `post_terminal`.
fn terminal_states(automaton: &Automaton) -> BTreeSet<String> {
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    for action in &automaton.actions {
        if let Some(action_sources) = legal_sources(action) {
            sources.extend(action_sources);
        }
    }
    automaton
        .automaton
        .states
        .iter()
        .filter(|state| !sources.contains(state.as_str()))
        .cloned()
        .collect()
}

/// The parts of the automaton the checker consults, resolved once.
pub(super) struct SpecView<'a> {
    pub(super) entity_name: &'a str,
    pub(super) declared: BTreeMap<&'a str, &'a Action>,
    terminal_states: BTreeSet<String>,
}

impl<'a> SpecView<'a> {
    pub(super) fn new(automaton: &'a Automaton) -> Self {
        Self {
            entity_name: automaton.automaton.name.as_str(),
            declared: automaton
                .actions
                .iter()
                .map(|action| (action.name.as_str(), action))
                .collect(),
            terminal_states: terminal_states(automaton),
        }
    }

    /// Legal source states for an action, or `None` when it declares no state
    /// precondition at all.
    fn legal_sources(&self, action: &'a Action) -> Option<BTreeSet<&'a str>> {
        legal_sources(action)
    }

    /// Classify an action name this actor's spec does not declare.
    pub(super) fn classify_undeclared(&self, action: &str) -> ViolationKind {
        if KERNEL_PLATFORM_ACTIONS.contains(&action) {
            ViolationKind::ForbiddenAction
        } else {
            ViolationKind::UnknownAction
        }
    }
}

/// State carried across the row stream.
#[derive(Default)]
pub(super) struct Walk {
    actor_rows: usize,
    platform_rows_skipped: usize,
    other_entity_rows_skipped: usize,
    transitions_unchecked: usize,
    /// Index at which each entity first reached a terminal state.
    terminal_at: BTreeMap<String, usize>,
    /// Index of each entity's most recent successful status change.
    last_state_change: BTreeMap<String, usize>,
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
            transitions_unchecked: self.transitions_unchecked,
            ots_decisions_checked: 0,
            terminal_entities: self.terminal_at.len(),
            violations_by_kind: BTreeMap::new(),
        }
    }
}
