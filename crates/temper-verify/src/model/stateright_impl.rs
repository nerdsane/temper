//! Stateright `Model` implementation for `TemperModel`.
//!
//! Implements `init_states`, `actions`, `next_state`, and `properties` to
//! enable exhaustive model checking via Stateright.

use stateright::{Model, Property};
use super::types::{
    InvariantKind, TemperModel, TemperModelAction, TemperModelState,
};

// -- Property condition functions (bare fn pointers) --------------------------
//
// Stateright requires `fn(&M, &M::State) -> bool`, so we define standalone
// functions that read invariant configuration from the model.

/// Check that the current status is in the set of valid states (TypeInvariant).
fn check_status_in_set(model: &TemperModel, state: &TemperModelState) -> bool {
    model.states.contains(&state.status)
}

/// Check that when status is in a trigger set, item_count > 0.
/// This function checks ALL ItemCountPositive invariants.
fn check_item_count_positive(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !matches!(inv.kind, InvariantKind::ItemCountPositive) {
            continue;
        }
        if inv.trigger_states.contains(&state.status) && state.item_count == 0 {
            return false;
        }
    }
    true
}

/// Check all implication invariants: when status is in trigger_states,
/// it must also be in required_states.
///
/// If required_states is empty, or none of the required_states are valid
/// order statuses, the invariant is trivially true (it constrains a variable
/// other than order status, like payment_status, which we don't model).
fn check_implications(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !matches!(inv.kind, InvariantKind::Implication) {
            continue;
        }
        if inv.trigger_states.contains(&state.status) {
            // Filter required_states to only those that are valid order statuses.
            // If an invariant's RHS references a non-status variable (like
            // payment_status), those values won't be in model.states and the
            // invariant is trivially satisfied (we can't check it).
            let valid_required: Vec<&String> = inv
                .required_states
                .iter()
                .filter(|s| model.states.contains(s))
                .collect();

            if valid_required.is_empty() {
                continue; // Trivially true (constrains non-status variables)
            }
            if !valid_required.contains(&&state.status) {
                return false;
            }
        }
    }
    true
}

impl Model for TemperModel {
    type State = TemperModelState;
    type Action = TemperModelAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![TemperModelState {
            status: self.initial_status.clone(),
            item_count: 0,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for t in &self.transitions {
            // A transition is enabled if its from_states list is empty (always
            // enabled) or the current status is in the from_states list.
            let status_ok = t.from_states.is_empty()
                || t.from_states.iter().any(|s| s == &state.status);

            if !status_ok {
                continue;
            }

            // For "add item" transitions, enforce the max_items bound.
            if t.is_add_item && state.item_count >= self.max_items {
                continue;
            }

            // For "remove item" transitions, require at least one item.
            if t.modifies_items && !t.is_add_item && state.item_count == 0 {
                continue;
            }

            // Transitions requiring items (e.g. SubmitOrder) need item_count > 0.
            if t.requires_items && state.item_count == 0 {
                continue;
            }

            actions.push(TemperModelAction {
                name: t.name.clone(),
                target_state: t.to_state.clone(),
            });
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        // Find the matching resolved transition.
        let resolved = self.transitions.iter().find(|t| t.name == action.name)?;

        let new_status = action
            .target_state
            .unwrap_or_else(|| state.status.clone());

        let new_item_count = if resolved.is_add_item {
            state.item_count + 1
        } else if resolved.modifies_items && !resolved.is_add_item {
            state.item_count.saturating_sub(1)
        } else {
            state.item_count
        };

        Some(TemperModelState {
            status: new_status,
            item_count: new_item_count,
        })
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = Vec::new();

        // Check if we have a StatusInSet invariant.
        let has_status_check = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::StatusInSet));
        if has_status_check {
            props.push(Property::always("TypeInvariant", check_status_in_set));
        }

        // Check if we have any ItemCountPositive invariants.
        let has_item_check = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::ItemCountPositive));
        if has_item_check {
            props.push(Property::always(
                "ItemCountInvariants",
                check_item_count_positive,
            ));
        }

        // Check if we have any Implication invariants.
        let has_implication = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Implication));
        if has_implication {
            props.push(Property::always(
                "ImplicationInvariants",
                check_implications,
            ));
        }

        props
    }
}
