//! Generate Stateright models from TLA+ StateMachine definitions.
//!
//! This module translates a `temper_spec::tlaplus::StateMachine` into a
//! Stateright `Model` that can be exhaustively explored by a model checker.
//! The generated model captures:
//!   - Status-based states with an item counter
//!   - Transitions as named actions with source/target state guards
//!   - Safety invariants as Stateright "always" properties
//!
//! Because Stateright's `Property::always` requires a bare function pointer
//! (not a capturing closure), all invariant data lives inside `TemperModel`
//! and is accessed via the `&TemperModel` reference in property conditions.

use std::fmt;
use stateright::{Model, Property};
use temper_spec::tlaplus::StateMachine;

/// The state tracked by the Temper model during verification.
///
/// Consists of the current entity status (e.g. "Draft", "Submitted") and a
/// simple item counter that tracks how many items have been added.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelState {
    /// Current status value (mirrors the TLA+ `status` variable).
    pub status: String,
    /// Number of items currently in the entity (simplified from the TLA+ set).
    pub item_count: usize,
}

impl fmt::Display for TemperModelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(items={})", self.status, self.item_count)
    }
}

/// An action that the model can take, corresponding to a TLA+ transition.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelAction {
    /// The transition name (e.g. "SubmitOrder", "CancelOrder").
    pub name: String,
    /// The target status after taking this action (if deterministic).
    pub target_state: Option<String>,
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target_state {
            Some(target) => write!(f, "{} -> {}", self.name, target),
            None => write!(f, "{}", self.name),
        }
    }
}

/// A resolved transition used internally by the model, pre-computed from a
/// TLA+ `Transition` for efficient matching during state exploration.
#[derive(Clone, Debug)]
struct ResolvedTransition {
    /// The action name.
    name: String,
    /// States from which this transition can fire.
    from_states: Vec<String>,
    /// The target state (if deterministic).
    to_state: Option<String>,
    /// Whether this transition modifies the item count.
    modifies_items: bool,
    /// Whether this is an "add item" action (increments counter).
    is_add_item: bool,
    /// Whether this transition requires item_count > 0 to fire.
    requires_items: bool,
}

/// The kind of check an invariant performs.
#[derive(Clone, Debug)]
pub enum InvariantKind {
    /// status must be in a known set of states.
    StatusInSet,
    /// When status is in trigger_states, item_count must be > 0.
    ItemCountPositive,
    /// When status is in trigger_states, status must also be in required_states.
    Implication,
}

/// A safety invariant resolved for runtime checking.
#[derive(Clone, Debug)]
pub struct ResolvedInvariant {
    /// The invariant name.
    pub name: String,
    /// States in which this invariant's check is activated (empty = always).
    pub trigger_states: Vec<String>,
    /// For implication invariants: the set of valid target states.
    pub required_states: Vec<String>,
    /// The kind of check this invariant performs.
    pub kind: InvariantKind,
}

/// The Stateright model generated from a TLA+ `StateMachine`.
///
/// This struct holds all the pre-computed transition and invariant data needed
/// to implement the `Model` trait efficiently. Invariant data is stored here
/// (rather than captured in closures) because Stateright's `Property::always`
/// requires a bare `fn` pointer.
#[derive(Clone)]
pub struct TemperModel {
    /// All valid status values from the specification.
    pub states: Vec<String>,
    /// Pre-resolved transitions.
    transitions: Vec<ResolvedTransition>,
    /// Pre-resolved safety invariants (accessible to property fn pointers via &self).
    pub invariants: Vec<ResolvedInvariant>,
    /// The initial status (first state from Init, typically "Draft").
    initial_status: String,
    /// Maximum item count for bounded exploration.
    max_items: usize,
}

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

/// Build a `TemperModel` from a parsed TLA+ `StateMachine`.
///
/// The model uses a bounded item count (default: 2) to keep the state space
/// finite and tractable for exhaustive model checking.
pub fn build_model(sm: &StateMachine) -> TemperModel {
    build_model_with_max_items(sm, 2)
}

/// Build a `TemperModel` with a custom maximum item count.
pub fn build_model_with_max_items(sm: &StateMachine, max_items: usize) -> TemperModel {
    build_model_impl(sm, max_items, None)
}

/// Build a `TemperModel` from the raw TLA+ source, which allows resolving
/// `CanXxx` guard predicates that aren't extracted as transitions.
pub fn build_model_from_tla(tla_source: &str, max_items: usize) -> TemperModel {
    let sm = temper_spec::tlaplus::extract_state_machine(tla_source)
        .expect("failed to extract state machine from TLA+ source");
    build_model_impl(&sm, max_items, Some(tla_source))
}

fn build_model_impl(
    sm: &StateMachine,
    max_items: usize,
    tla_source: Option<&str>,
) -> TemperModel {
    let guard_map = build_guard_map(sm, tla_source);
    let transitions = resolve_transitions(sm, &guard_map);
    let invariants = resolve_invariants(sm);

    // The initial status is typically the first state listed, but we prefer
    // "Draft" if present (matching the common TLA+ Init pattern).
    let initial_status = if sm.states.contains(&"Draft".to_string()) {
        "Draft".to_string()
    } else {
        sm.states.first().cloned().unwrap_or_else(|| "Unknown".to_string())
    };

    TemperModel {
        states: sm.states.clone(),
        transitions,
        invariants,
        initial_status,
        max_items,
    }
}

/// Information extracted from a `CanXxx` guard predicate.
#[derive(Clone, Debug, Default)]
struct GuardInfo {
    /// States from which the guarded transition can fire.
    from_states: Vec<String>,
    /// Whether the guard requires Cardinality(items) > 0.
    requires_items: bool,
}

/// Build a map from `CanXxx` guard predicate names to their `GuardInfo`.
///
/// This extracts guard definitions from either:
/// - Transitions in the StateMachine that start with "Can" (those parsed by
///   the extractor, e.g., CanRemoveItem)
/// - The raw TLA+ source (for guard definitions that the extractor skipped
///   because they have no parameters, e.g., CanSubmit, CanShip)
fn build_guard_map(
    sm: &StateMachine,
    tla_source: Option<&str>,
) -> std::collections::HashMap<String, GuardInfo> {
    let mut guard_map: std::collections::HashMap<String, GuardInfo> =
        std::collections::HashMap::new();

    // From transitions already extracted.
    for t in &sm.transitions {
        if t.name.starts_with("Can") && !t.from_states.is_empty() {
            guard_map.insert(
                t.name.clone(),
                GuardInfo {
                    from_states: t.from_states.clone(),
                    requires_items: body_requires_positive_items(&t.guard_expr),
                },
            );
        }
    }

    // Parse the raw TLA+ source for CanXxx guard definitions that weren't
    // extracted as transitions.
    if let Some(source) = tla_source {
        parse_guard_definitions(source, &sm.states, &mut guard_map);
    }

    guard_map
}

/// Parse `CanXxx ==` definitions from TLA+ source and extract their GuardInfo.
fn parse_guard_definitions(
    source: &str,
    all_states: &[String],
    guard_map: &mut std::collections::HashMap<String, GuardInfo>,
) {
    let mut current_guard: Option<String> = None;
    let mut current_body = String::new();

    for line in source.lines() {
        let trimmed = line.trim();

        // Skip comments.
        if trimmed.starts_with("\\*") {
            continue;
        }

        // Detect guard definitions: CanXxx == or CanXxx(param) ==
        if trimmed.contains("==") {
            let name_part = trimmed.split("==").next().unwrap_or("").trim();
            let name = name_part.split('(').next().unwrap_or("").trim();

            if name.starts_with("Can") && !name.is_empty() {
                // Save previous guard.
                if let Some(prev_name) = current_guard.take() {
                    if !guard_map.contains_key(&prev_name) {
                        let info = extract_guard_info(&current_body, all_states);
                        guard_map.insert(prev_name, info);
                    }
                }

                current_guard = Some(name.to_string());
                current_body.clear();
                // Include text after ==
                if let Some(rest) = trimmed.split("==").nth(1) {
                    current_body.push_str(rest.trim());
                    current_body.push('\n');
                }
                continue;
            }

            // Non-Can definition ends the current guard.
            if current_guard.is_some()
                && !trimmed.starts_with("/\\")
                && !trimmed.starts_with("\\/")
            {
                if let Some(prev_name) = current_guard.take() {
                    if !guard_map.contains_key(&prev_name) {
                        let info = extract_guard_info(&current_body, all_states);
                        guard_map.insert(prev_name, info);
                    }
                }
                continue;
            }
        }

        if current_guard.is_some() {
            current_body.push_str(trimmed);
            current_body.push('\n');
        }
    }

    // Final guard.
    if let Some(prev_name) = current_guard.take() {
        if !guard_map.contains_key(&prev_name) {
            let info = extract_guard_info(&current_body, all_states);
            guard_map.insert(prev_name, info);
        }
    }
}

/// Extract guard info from a guard body.
fn extract_guard_info(body: &str, all_states: &[String]) -> GuardInfo {
    let from_states = extract_states_from_guard_body(body, all_states);
    let requires_items = body_requires_positive_items(body);

    GuardInfo {
        from_states,
        requires_items,
    }
}

/// Check if a guard body requires item_count > 0.
/// Matches patterns like `Cardinality(items) > 0` or `items /= {}` but NOT
/// `Cardinality(items) < MAX_ITEMS` (which is a bound, not a requirement).
fn body_requires_positive_items(body: &str) -> bool {
    // Look for explicit "Cardinality(items) > 0" pattern.
    if body.contains("Cardinality(items) > 0") {
        return true;
    }
    // Look for "item \in items" which implies items is non-empty.
    if body.contains("item \\in items") {
        return true;
    }
    false
}

/// Extract from_states from a guard body by looking for `status = "X"` or
/// `status \in {"X", "Y"}` patterns.
fn extract_states_from_guard_body(body: &str, all_states: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("status =") || trimmed.contains("status="))
            && !trimmed.contains("status'")
        {
            if let Some(s) = extract_quoted(trimmed) {
                if all_states.contains(&s) && !found.contains(&s) {
                    found.push(s);
                }
            }
        }
        if trimmed.contains("status \\in") {
            for s in extract_quoted_set(trimmed) {
                if all_states.contains(&s) && !found.contains(&s) {
                    found.push(s);
                }
            }
        }
    }
    found
}

/// Convert TLA+ transitions into resolved transitions for the model.
///
/// The TLA+ extractor may produce transitions with empty `from_states` when
/// the guard references a `CanXxx` predicate instead of inlining the status
/// check. This function resolves those references by:
/// 1. Building a map from `CanXxx` guard names to their from_states
/// 2. Looking up `CanXxx` references in each transition's guard expression
/// 3. Filtering out guard definitions (names starting with "Can") that are not
///    real actions
fn resolve_transitions(
    sm: &StateMachine,
    guard_map: &std::collections::HashMap<String, GuardInfo>,
) -> Vec<ResolvedTransition> {
    sm.transitions
        .iter()
        .filter(|t| {
            // Filter out guard definitions (CanXxx). These are predicate
            // definitions in TLA+, not actual transitions.
            !t.name.starts_with("Can")
        })
        .map(|t| {
            let name_lower = t.name.to_lowercase();
            let effect_lower = t.effect_expr.to_lowercase();

            let is_add_item = name_lower.contains("additem")
                || name_lower.contains("add_item")
                || (effect_lower.contains("items'") && effect_lower.contains("union"));

            let is_remove_item = name_lower.contains("removeitem")
                || name_lower.contains("remove_item")
                || (effect_lower.contains("items'") && effect_lower.contains("\\"));

            let modifies_items = is_add_item || is_remove_item;

            // Resolve guard info via CanXxx references.
            let resolved_guard = resolve_guard_info(&t.guard_expr, guard_map, &sm.states);

            let from_states = if t.from_states.is_empty() {
                resolved_guard.from_states
            } else {
                t.from_states.clone()
            };

            let requires_items = resolved_guard.requires_items;

            ResolvedTransition {
                name: t.name.clone(),
                from_states,
                to_state: t.to_state.clone(),
                modifies_items,
                is_add_item,
                requires_items,
            }
        })
        .collect()
}

/// Resolve guard information by looking for CanXxx predicate references or
/// direct status checks in a guard expression.
fn resolve_guard_info(
    guard_expr: &str,
    guard_map: &std::collections::HashMap<String, GuardInfo>,
    all_states: &[String],
) -> GuardInfo {
    // Check for direct CanXxx references in the guard.
    for (guard_name, info) in guard_map {
        if guard_expr.contains(guard_name.as_str()) {
            return info.clone();
        }
    }

    // Fallback: look for direct status checks in the guard expression.
    let mut found = Vec::new();
    for line in guard_expr.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("status =") || trimmed.contains("status="))
            && !trimmed.contains("status'")
        {
            if let Some(s) = extract_quoted(trimmed) {
                if all_states.contains(&s) && !found.contains(&s) {
                    found.push(s);
                }
            }
        }
        if trimmed.contains("status \\in") {
            for s in extract_quoted_set(trimmed) {
                if all_states.contains(&s) && !found.contains(&s) {
                    found.push(s);
                }
            }
        }
    }

    GuardInfo {
        from_states: found,
        requires_items: body_requires_positive_items(guard_expr),
    }
}

/// Convert TLA+ invariants into resolved invariants for the model.
fn resolve_invariants(sm: &StateMachine) -> Vec<ResolvedInvariant> {
    let mut result = Vec::new();

    for inv in &sm.invariants {
        let expr = &inv.expr;

        // Detect "TypeInvariant" -- status must be in a known set.
        if inv.name.contains("TypeInvariant")
            || (inv.name.contains("Type") && expr.contains("\\in"))
        {
            result.push(ResolvedInvariant {
                name: inv.name.clone(),
                trigger_states: vec![],
                required_states: vec![],
                kind: InvariantKind::StatusInSet,
            });
            continue;
        }

        // Detect item-count invariants like "SubmitRequiresItems".
        if expr.contains("Cardinality(items)")
            || (expr.contains("items") && expr.contains("> 0"))
        {
            let triggers = extract_trigger_states(expr);
            result.push(ResolvedInvariant {
                name: inv.name.clone(),
                trigger_states: triggers,
                required_states: vec![],
                kind: InvariantKind::ItemCountPositive,
            });
            continue;
        }

        // Detect implication invariants: status = "X" => some_condition
        if expr.contains("=>") {
            let triggers = extract_trigger_states(expr);
            let required = extract_required_states(expr);
            result.push(ResolvedInvariant {
                name: inv.name.clone(),
                trigger_states: triggers,
                required_states: required,
                kind: InvariantKind::Implication,
            });
            continue;
        }

        // Fallback: treat as a generic implication with triggers from the expr.
        let triggers = extract_trigger_states(expr);
        result.push(ResolvedInvariant {
            name: inv.name.clone(),
            trigger_states: triggers,
            required_states: vec![],
            kind: InvariantKind::Implication,
        });
    }

    result
}

/// Extract trigger states from an invariant expression.
/// Looks for patterns like `status = "X"` or `status \in {"X", "Y"}` on the
/// left side of `=>`.
fn extract_trigger_states(expr: &str) -> Vec<String> {
    let lhs = if let Some(idx) = expr.find("=>") {
        &expr[..idx]
    } else {
        expr
    };

    let mut states = Vec::new();

    for line in lhs.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("status =") || trimmed.contains("status="))
            && !trimmed.contains("status'")
        {
            if let Some(s) = extract_quoted(trimmed) {
                if !states.contains(&s) {
                    states.push(s);
                }
            }
        }
        if trimmed.contains("status \\in") {
            for s in extract_quoted_set(trimmed) {
                if !states.contains(&s) {
                    states.push(s);
                }
            }
        }
    }

    states
}

/// Extract required states from the right side of an implication.
fn extract_required_states(expr: &str) -> Vec<String> {
    let rhs = if let Some(idx) = expr.find("=>") {
        &expr[idx + 2..]
    } else {
        return vec![];
    };

    let mut states = Vec::new();
    for line in rhs.lines() {
        let trimmed = line.trim();
        if trimmed.contains("status") {
            if let Some(s) = extract_quoted(trimmed) {
                if !states.contains(&s) {
                    states.push(s);
                }
            }
            for s in extract_quoted_set(trimmed) {
                if !states.contains(&s) {
                    states.push(s);
                }
            }
        }
    }

    states.sort();
    states.dedup();
    states
}

/// Extract a single quoted string from a line.
fn extract_quoted(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract all quoted strings from a set expression like `{"X", "Y"}`.
fn extract_quoted_set(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut word = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                word.push(c);
            }
            if !word.is_empty() && !result.contains(&word) {
                result.push(word);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_TLA: &str = include_str!("../../../reference/ecommerce/specs/order.tla");

    fn build_order_model() -> TemperModel {
        build_model_from_tla(ORDER_TLA, 2)
    }

    #[test]
    fn test_build_model_has_correct_states() {
        let model = build_order_model();
        assert_eq!(model.states.len(), 10);
        assert!(model.states.contains(&"Draft".to_string()));
        assert!(model.states.contains(&"Submitted".to_string()));
        assert!(model.states.contains(&"Confirmed".to_string()));
        assert!(model.states.contains(&"Refunded".to_string()));
    }

    #[test]
    fn test_build_model_initial_state_is_draft() {
        let model = build_order_model();
        let init = model.init_states();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].status, "Draft");
        assert_eq!(init[0].item_count, 0);
    }

    #[test]
    fn test_draft_actions_include_add_item() {
        let model = build_order_model();
        let state = TemperModelState {
            status: "Draft".to_string(),
            item_count: 0,
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"AddItem"),
            "Draft state should allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_submitted_does_not_allow_add_item() {
        let model = build_order_model();
        let state = TemperModelState {
            status: "Submitted".to_string(),
            item_count: 1,
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            !names.contains(&"AddItem"),
            "Submitted state should NOT allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_draft_to_submitted_transition() {
        let model = build_order_model();
        let state = TemperModelState {
            status: "Draft".to_string(),
            item_count: 1,
        };
        let action = TemperModelAction {
            name: "SubmitOrder".to_string(),
            target_state: Some("Submitted".to_string()),
        };
        let next = model.next_state(&state, action);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.status, "Submitted");
        assert_eq!(next.item_count, 1);
    }

    #[test]
    fn test_submitted_to_confirmed_transition() {
        let model = build_order_model();
        let state = TemperModelState {
            status: "Submitted".to_string(),
            item_count: 1,
        };
        let action = TemperModelAction {
            name: "ConfirmOrder".to_string(),
            target_state: Some("Confirmed".to_string()),
        };
        let next = model.next_state(&state, action);
        assert!(next.is_some());
        assert_eq!(next.unwrap().status, "Confirmed");
    }

    #[test]
    fn test_add_item_increments_count() {
        let model = build_order_model();
        let state = TemperModelState {
            status: "Draft".to_string(),
            item_count: 0,
        };
        let action = TemperModelAction {
            name: "AddItem".to_string(),
            target_state: None,
        };
        let next = model.next_state(&state, action).unwrap();
        assert_eq!(next.item_count, 1);
        assert_eq!(next.status, "Draft");
    }

    #[test]
    fn test_properties_are_generated() {
        let model = build_order_model();
        let props = model.properties();
        assert!(
            !props.is_empty(),
            "Model should have at least one property"
        );
    }
}
