//! Model builder: constructs a `TemperModel` from specification `StateMachine` definitions.
//!
//! Resolves guard predicates, transitions, and invariants from I/O Automaton or
//! TLA+ sources into pre-computed structures for efficient model checking.

use temper_spec::tlaplus::StateMachine;

use super::types::{
    InvariantKind, ResolvedInvariant, ResolvedTransition, TemperModel,
};

/// Build a `TemperModel` from a parsed specification `StateMachine`.
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

/// Build a `TemperModel` from I/O Automaton TOML source.
///
/// The IOA format has explicit guards, so no TLA+ source parsing is needed.
/// The automaton is converted to a `StateMachine` IR for compatibility.
pub fn build_model_from_ioa(ioa_toml: &str, max_items: usize) -> TemperModel {
    let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
        .expect("failed to parse I/O Automaton TOML");
    let sm = temper_spec::automaton::parser::to_state_machine(&automaton);
    build_model_impl(&sm, max_items, None)
}

fn build_model_impl(
    sm: &StateMachine,
    max_items: usize,
    tla_source: Option<&str>,
) -> TemperModel {
    let guard_map = build_guard_map(sm, tla_source);
    let transitions = resolve_transitions(sm, &guard_map);
    let invariants = resolve_invariants(sm);

    // Initial status: first state listed in the spec.
    let initial_status = sm.states.first().cloned().unwrap_or_default();

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
            // Filter out guard definitions (CanXxx without parameters).
            // Guards: CanSubmit, CanCancel (no parens in original TLA+)
            // NOT guards: CancelOrder (has parameters, is a real action)
            !(t.name.starts_with("Can") && !t.has_parameters)
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
    use stateright::Model;

    const ORDER_TLA: &str = include_str!("../../../../test-fixtures/specs/order.tla");

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
        let state = super::super::types::TemperModelState {
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
        let state = super::super::types::TemperModelState {
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
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            item_count: 1,
        };
        let action = super::super::types::TemperModelAction {
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
        let state = super::super::types::TemperModelState {
            status: "Submitted".to_string(),
            item_count: 1,
        };
        let action = super::super::types::TemperModelAction {
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
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            item_count: 0,
        };
        let action = super::super::types::TemperModelAction {
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

    #[test]
    fn debug_resolved_transitions() {
        let model = build_model_from_tla(ORDER_TLA, 2);
        for t in &model.transitions {
            eprintln!("{}: from={:?} to={:?} requires_items={}", t.name, t.from_states, t.to_state, t.requires_items);
        }
    }
}
