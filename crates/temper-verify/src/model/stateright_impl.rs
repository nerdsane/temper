//! Stateright `Model` implementation for `TemperModel`.
//!
//! Implements `init_states`, `actions`, `next_state`, and `properties` to
//! enable exhaustive model checking via Stateright. Supports multi-variable
//! state (counters + booleans), safety invariants, and deadlock freedom.
//! `reaches` and `can_reach` rules are checked on the whole walk instead.

use stateright::{Model, Property};

use super::semantics::{canonical_state, guard_may_hold, rule_for, step, successors, truth};
use temper_spec::predicate::Truth;

use super::parameters::locally_enabled;

use super::types::{
    LivenessKind, ResolvedTransition, TemperModel, TemperModelAction, TemperModelState,
};

/// The property that no move breaks an entry rule. It is read in the state
/// before the move, so its path ends with that move.
pub(crate) const ENTRY_RULES: &str = "EntryRules";

// -- Property condition functions (bare fn pointers) -------------------------

/// Check that no move enabled here enters a place with its entry rule false.
fn check_entry_rules(model: &TemperModel, state: &TemperModelState) -> bool {
    super::entry::entry_broken(model, state).is_none()
}

/// Check that the current status is a declared state.
fn check_status_in_set(model: &TemperModel, state: &TemperModelState) -> bool {
    model.states.iter().any(|s| *s == state.status)
}

/// Check that no invariant is false in this state. An invariant over values
/// the model does not track evaluates to unknown and is not a violation.
fn check_invariants(model: &TemperModel, state: &TemperModelState) -> bool {
    model
        .invariants
        .iter()
        .all(|inv| truth(&inv.assert, &model.var_kinds, state) != Truth::False)
}

/// Check that no transition is locally enabled from a terminal state.
fn check_terminal(model: &TemperModel, state: &TemperModelState) -> bool {
    if !model.terminal.iter().any(|s| *s == state.status) {
        return true;
    }
    !model.transitions.iter().any(|t| {
        let status_ok =
            t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
        status_ok && locally_enabled(model, t, state) == Some(true)
    })
}

// -- Liveness property functions ---------------------------------------------

/// Check liveness: from the specified states, at least one action is enabled.
/// (Deadlock freedom expressed as a safety property.)
fn check_no_deadlock(model: &TemperModel, state: &TemperModelState) -> bool {
    for live in &model.liveness {
        if let LivenessKind::NoDeadlock { ref from } = live.kind
            && from.iter().any(|s| *s == state.status)
        {
            // Must have at least one enabled action
            let mut has_action = false;
            for t in &model.transitions {
                let status_ok =
                    t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
                if status_ok && locally_enabled(model, t, state) != Some(false) {
                    has_action = true;
                    break;
                }
            }
            if !has_action {
                return false;
            }
        }
    }
    true
}

// -- Model trait implementation ----------------------------------------------

impl TemperModel {
    /// Whether `transition` is enabled in `state`: it is the transition the
    /// runtime takes for its action in this status ([`rule_for`]), and its
    /// guard may hold.
    ///
    /// The single definition of "this edge may fire here", so that the action
    /// generator, the dead-transition walk and any caller that applies an
    /// action out of band cannot drift apart on what the spec permits.
    pub(crate) fn transition_enabled(
        &self,
        transition: &ResolvedTransition,
        state: &TemperModelState,
    ) -> bool {
        let taken = rule_for(self, &transition.name, &state.status)
            .is_some_and(|rule| std::ptr::eq(rule, transition));
        // Guard checked for *fireability*: a cross-entity guard is a free
        // boolean, so `guard_may_hold` admits the gated edge rather than
        // pruning it. See `actions` for why that is the right direction.
        taken && guard_may_hold(&transition.guard, &self.var_kinds, state)
    }

    /// Whether a record in `state` surely has a move open: one the walk
    /// takes whose guard holds from the record alone. A move gated on
    /// another record, or on a value the model does not track, is explored
    /// as one that may fire, but it may never be open, so a record with no
    /// other may wait where it is.
    pub(crate) fn surely_moves(&self, state: &TemperModelState) -> bool {
        self.transitions.iter().any(|t| {
            self.transition_enabled(t, state) && locally_enabled(self, t, state) == Some(true)
        })
    }

    /// Whether `action` may fire in `state`.
    ///
    /// [`Model::next_state`] cannot answer this: it resolves the transition
    /// for the action's name and applies it unconditionally, which is sound under Stateright
    /// (every action it applies came from `actions` for that same state) but
    /// not for a caller that holds an action across a state change. The
    /// simulator does exactly that -- it chooses an action while an actor is in
    /// one state and delivers it, after a fault-injected delay, into another --
    /// so it must ask this first.
    ///
    /// An unknown action name is not enabled; there is no transition to fire.
    pub fn action_enabled(&self, state: &TemperModelState, action: &TemperModelAction) -> bool {
        rule_for(self, &action.name, &state.status).is_some_and(|t| {
            self.transition_enabled(t, state)
                && (self.entry_rules.is_empty()
                    || super::parameters::inputs_allow(self, t, state, &action.params))
        })
    }
}

impl Model for TemperModel {
    type State = TemperModelState;
    type Action = TemperModelAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![TemperModelState {
            status: self.initial_status,
            counters: self.initial_counters.clone(),
            booleans: self.initial_booleans.clone(),
            lists: self.initial_lists.clone(),
            strings: self.initial_strings.clone(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // A spent budget ends every walk here: Stateright's own timeout is not
        // seen by a single-threaded BFS until its whole queue is empty.
        if self.out_of_time() {
            return;
        }
        for t in &self.transitions {
            // Status precondition and guard, via the shared predicate that
            // `action_enabled` also uses -- see there for the guard rationale.
            if !self.transition_enabled(t, state) {
                continue;
            }

            // One action per parameter assignment whose step stays within
            // the exploration bounds.
            for (params, _) in successors(self, t, state) {
                actions.push(TemperModelAction {
                    name: t.name,
                    target_state: t.to_state,
                    params,
                });
            }
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        // The rule the runtime takes in this status gives both the effects
        // and the target: an action chosen in one state and delivered in
        // another (the simulator's late delivery) takes that state's rule.
        let resolved = rule_for(self, &action.name, &state.status)?;
        let mut next = step(resolved, state, &action.params);
        canonical_state(&mut next);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = Vec::new();

        props.push(Property::always("TypeInvariant", check_status_in_set));
        if !self.invariants.is_empty() {
            props.push(Property::always("Invariants", check_invariants));
        }
        if !self.entry_rules.is_empty() {
            props.push(Property::always(ENTRY_RULES, check_entry_rules));
        }
        if !self.terminal.is_empty() {
            props.push(Property::always("Terminal", check_terminal));
        }

        // Liveness: NoDeadlock (expressed as safety: "always has actions")
        let has_no_deadlock = self
            .liveness
            .iter()
            .any(|l| matches!(l.kind, LivenessKind::NoDeadlock { .. }));
        if has_no_deadlock {
            props.push(Property::always("NoDeadlock", check_no_deadlock));
        }

        // `reaches` rules are not Stateright properties: its `eventually`
        // sees only acyclic paths, so a loop that never gets there passes,
        // and it takes one fn for every rule. They are checked rule by rule
        // on the whole walk (`crate::reaches`).

        props
    }
}
