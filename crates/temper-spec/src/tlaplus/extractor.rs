use super::types::*;

#[derive(Debug, thiserror::Error)]
pub enum TlaExtractError {
    #[error("no MODULE declaration found")]
    NoModule,
    #[error("no state set found (expected a set assignment like States == {{...}})")]
    NoStates,
    #[error("parse error: {0}")]
    Parse(String),
}

/// Extract state machine structure from a TLA+ specification.
///
/// This is a pragmatic extractor, not a full TLA+ parser. It uses pattern
/// matching to find:
/// - MODULE name
/// - CONSTANTS and VARIABLES
/// - State set definitions (OrderStatuses == {...})
/// - Action definitions (Name == /\ guard /\ effect)
/// - Invariants (safety properties)
/// - Liveness properties (temporal formulas with ~>)
pub fn extract_state_machine(tla_source: &str) -> Result<StateMachine, TlaExtractError> {
    let module_name = extract_module_name(tla_source)?;
    let constants = extract_list_after(tla_source, "CONSTANTS");
    let variables = extract_list_after(tla_source, "VARIABLES");
    let states = extract_states(tla_source)?;
    let transitions = extract_transitions(tla_source, &states);
    let invariants = extract_invariants(tla_source);
    let liveness_properties = extract_liveness(tla_source);

    Ok(StateMachine {
        module_name,
        states,
        transitions,
        invariants,
        liveness_properties,
        constants,
        variables,
    })
}

fn extract_module_name(source: &str) -> Result<String, TlaExtractError> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---- MODULE") || trimmed.starts_with("---- MODULE") {
            // Format: ---- MODULE Name ----
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 3 {
                return Ok(parts[2].trim_end_matches('-').trim().to_string());
            }
        }
    }
    Err(TlaExtractError::NoModule)
}

fn extract_list_after(source: &str, keyword: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_section = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(keyword) {
            in_section = true;
            // Items may be on the same line
            let after = trimmed.strip_prefix(keyword).unwrap_or("").trim();
            for item in after.split(',') {
                let item = item.trim().trim_matches(|c| c == '\\' || c == '*');
                let item = item.trim();
                if !item.is_empty() {
                    result.push(item.to_string());
                }
            }
            continue;
        }
        if in_section {
            if trimmed.is_empty() || trimmed.starts_with("VARIABLE") || trimmed.starts_with("----") {
                break;
            }
            // Strip TLA+ line comments (\*)
            let without_comment = if let Some(idx) = trimmed.find("\\*") {
                &trimmed[..idx]
            } else {
                trimmed
            };
            for item in without_comment.split(',') {
                let item = item.trim().trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if !item.is_empty() {
                    result.push(item.to_string());
                }
            }
        }
    }

    result
}

fn extract_states(source: &str) -> Result<Vec<String>, TlaExtractError> {
    // Look for the FIRST pattern like: XxxStatuses == {"Draft", "Submitted", ...}
    // The first one is the primary entity status set. Subsequent ones (PaymentStatuses,
    // ShipmentStatuses) are auxiliary and should not be included.
    let mut states = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("Statuses ==") || trimmed.contains("States =="))
            && trimmed.contains('{')
        {
            states = extract_string_set(source, trimmed);
            break; // Take only the first status set
        }
    }

    // Also look for state references in status variable assignments
    if states.is_empty() {
        // Fallback: look for status = "xxx" patterns in Init and transitions
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.contains("status =") || trimmed.contains("status' =") {
                if let Some(s) = extract_quoted_string(trimmed) {
                    if !states.contains(&s) {
                        states.push(s);
                    }
                }
            }
            if trimmed.contains("status \\in") {
                states.extend(extract_inline_set(trimmed));
            }
        }
    }

    if states.is_empty() {
        return Err(TlaExtractError::NoStates);
    }

    Ok(states)
}

fn extract_string_set(source: &str, start_line: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut collecting = false;
    let mut buffer = String::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == start_line || (collecting && !buffer.contains('}')) {
            collecting = true;
            buffer.push_str(trimmed);
            buffer.push(' ');
        }
        if collecting && buffer.contains('}') {
            break;
        }
    }

    // Extract quoted strings from the buffer
    let mut chars = buffer.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                s.push(c);
            }
            if !s.is_empty() {
                result.push(s);
            }
        }
    }

    result
}

/// Check if a line is a section boundary that terminates transition extraction.
fn is_section_boundary(trimmed: &str) -> bool {
    trimmed.starts_with("\\*")
        && (trimmed.contains("Safety Invariant") || trimmed.contains("Liveness Propert"))
}

/// Check if a line is a Next-state relation (terminates transition extraction).
fn is_next_state_relation(trimmed: &str) -> bool {
    trimmed.starts_with("Next") && trimmed.contains("==")
}

/// Check if a line is an action definition (has `==` and is not a guard/init/meta).
fn is_action_definition(trimmed: &str) -> bool {
    trimmed.contains(" ==")
        && !trimmed.contains("Statuses ==")
        && !trimmed.contains("States ==")
        && !trimmed.contains("vars ==")
        && !is_guard_definition(trimmed)
        && !trimmed.starts_with("Init ==")
        && !trimmed.starts_with("Next")
        && !trimmed.starts_with("Spec")
        && !trimmed.starts_with("ASSUME")
}

/// Extract an action name from a definition line, preserving parens for has_parameters.
fn extract_action_name(trimmed: &str) -> Option<String> {
    let name_part = trimmed.split("==").next().unwrap_or("").trim();
    let clean = name_part.split('(').next().unwrap_or(name_part).trim();
    if !clean.is_empty() && clean.chars().next().map_or(false, |c| c.is_uppercase()) {
        Some(name_part.to_string())
    } else {
        None
    }
}

/// Save a completed action as a Transition (helper to reduce repetition).
fn save_action(
    name: &str, guard: &str, effect: &str, states: &[String], out: &mut Vec<Transition>,
) {
    out.push(build_transition(name, guard, effect, states));
}

fn extract_transitions(source: &str, states: &[String]) -> Vec<Transition> {
    let mut transitions = Vec::new();
    let mut current_action: Option<String> = None;
    let mut current_guard = String::new();
    let mut current_effect = String::new();
    let mut in_action = false;

    for line in source.lines() {
        let trimmed = line.trim();

        if is_section_boundary(trimmed) || is_next_state_relation(trimmed) {
            if let Some(name) = current_action.take() {
                save_action(&name, &current_guard, &current_effect, states, &mut transitions);
            }
            break;
        }

        if trimmed.starts_with("\\*") {
            continue;
        }

        if is_action_definition(trimmed) {
            if let Some(name) = current_action.take() {
                save_action(&name, &current_guard, &current_effect, states, &mut transitions);
            }
            if let Some(action_name) = extract_action_name(trimmed) {
                current_action = Some(action_name);
                current_guard.clear();
                current_effect.clear();
                in_action = true;
                if let Some(rest) = trimmed.split("==").nth(1) {
                    categorize_line(rest.trim(), &mut current_guard, &mut current_effect);
                }
            }
            continue;
        }

        if in_action {
            if trimmed.contains(" ==") && !trimmed.starts_with("/\\") && !trimmed.starts_with("\\/") {
                if let Some(name) = current_action.take() {
                    save_action(&name, &current_guard, &current_effect, states, &mut transitions);
                }
                in_action = false;
                continue;
            }
            categorize_line(trimmed, &mut current_guard, &mut current_effect);
        }
    }

    if let Some(name) = current_action.take() {
        save_action(&name, &current_guard, &current_effect, states, &mut transitions);
    }
    transitions
}

/// Guard definitions start with "Can" and are simple predicate names without parameters.
/// They don't modify state (no primed variables). Actions like CancelOrder have parameters.
fn is_guard_definition(line: &str) -> bool {
    let name = line.split("==").next().unwrap_or("").trim();
    // Guards: CanSubmit ==, CanCancel ==
    // NOT guards: CancelOrder(reason) ==
    name.starts_with("Can") && !name.contains('(')
}

fn categorize_line(line: &str, guard: &mut String, effect: &mut String) {
    let cleaned = line.trim().trim_start_matches("/\\").trim();
    if cleaned.contains("UNCHANGED") || cleaned.contains("' =") || cleaned.contains("'=") {
        effect.push_str(cleaned);
        effect.push('\n');
    } else {
        guard.push_str(cleaned);
        guard.push('\n');
    }
}

fn build_transition(
    name: &str,
    guard: &str,
    effect: &str,
    states: &[String],
) -> Transition {
    let from_states = extract_from_states(guard, states);
    let to_state = extract_to_state(effect, states);
    // Check if the raw action definition had parameters by looking for
    // parenthesized patterns in the guard (e.g., "CancelOrder(reason)")
    // Since the name may already be cleaned, also check the guard for param patterns
    let has_parameters = name.contains('(')
        || guard.contains("\\E reason \\in")
        || guard.contains("\\E item \\in")
        || effect.contains("reason'")
        || effect.contains("return_reason'");
    let clean_name = name.split('(').next().unwrap_or(name).to_string();

    Transition {
        name: clean_name,
        from_states,
        to_state,
        guard_expr: guard.trim().to_string(),
        has_parameters,
        effect_expr: effect.trim().to_string(),
    }
}

fn extract_from_states(guard: &str, states: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    // Pattern: status = "Xxx" or status \in {"Xxx", "Yyy"}
    for line in guard.lines() {
        let trimmed = line.trim();

        if trimmed.contains("status =") && !trimmed.contains("status' =") {
            if let Some(s) = extract_quoted_string(trimmed) {
                if states.contains(&s) && !result.contains(&s) {
                    result.push(s);
                }
            }
        }

        if trimmed.contains("status \\in") {
            for s in extract_inline_set(trimmed) {
                if states.contains(&s) && !result.contains(&s) {
                    result.push(s);
                }
            }
        }
    }

    // Also check for references like CanXxx which refer to guard definitions
    // (we handle this by checking the guard expression contains state checks)

    result
}

fn extract_to_state(effect: &str, states: &[String]) -> Option<String> {
    // Pattern: status' = "Xxx"
    for line in effect.lines() {
        let trimmed = line.trim();
        if trimmed.contains("status'") && trimmed.contains('=') {
            if let Some(s) = extract_quoted_string(trimmed) {
                if states.contains(&s) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn extract_quoted_string(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_inline_set(s: &str) -> Vec<String> {
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
            if !word.is_empty() {
                result.push(word);
            }
        }
    }
    result
}

fn extract_invariants(source: &str) -> Vec<Invariant> {
    let mut invariants = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_expr = String::new();

    // Look for named invariants in the safety section
    let mut in_invariant_section = false;

    for line in source.lines() {
        let trimmed = line.trim();

        // Detect invariant-like definitions
        if trimmed.starts_with("\\*") && trimmed.contains("Safety Invariant") {
            in_invariant_section = true;
            continue;
        }

        if trimmed.starts_with("\\*") && trimmed.contains("Liveness") {
            in_invariant_section = false;
            continue;
        }

        // Named invariants: InvariantName == expr
        if in_invariant_section
            && trimmed.contains(" ==")
            && !trimmed.starts_with("\\*")
            && !trimmed.starts_with("SafetyInvariant")
        {
            // Save previous
            if let Some(name) = current_name.take() {
                invariants.push(Invariant {
                    name,
                    expr: current_expr.trim().to_string(),
                });
            }

            let parts: Vec<&str> = trimmed.splitn(2, "==").collect();
            if parts.len() == 2 {
                current_name = Some(parts[0].trim().to_string());
                current_expr = parts[1].trim().to_string();
                current_expr.push('\n');
            }
            continue;
        }

        if in_invariant_section && current_name.is_some() {
            if trimmed.is_empty() || (trimmed.contains(" ==") && !trimmed.starts_with("/\\")) {
                if let Some(name) = current_name.take() {
                    invariants.push(Invariant {
                        name,
                        expr: current_expr.trim().to_string(),
                    });
                    current_expr.clear();
                }
            } else {
                current_expr.push_str(trimmed);
                current_expr.push('\n');
            }
        }
    }

    // Final one
    if let Some(name) = current_name {
        invariants.push(Invariant {
            name,
            expr: current_expr.trim().to_string(),
        });
    }

    invariants
}

fn extract_liveness(source: &str) -> Vec<LivenessProperty> {
    let mut properties = Vec::new();
    let mut in_liveness = false;
    let mut current_name: Option<String> = None;
    let mut current_expr = String::new();

    for line in source.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("\\*") && trimmed.contains("Liveness") {
            in_liveness = true;
            continue;
        }

        if trimmed.starts_with("\\*") && in_liveness
            && (trimmed.contains("Specification") || trimmed.contains("Model checking"))
        {
            in_liveness = false;
        }

        if in_liveness && trimmed.contains(" ==") && !trimmed.starts_with("\\*") {
            if let Some(name) = current_name.take() {
                properties.push(LivenessProperty {
                    name,
                    expr: current_expr.trim().to_string(),
                });
            }

            let parts: Vec<&str> = trimmed.splitn(2, "==").collect();
            if parts.len() == 2 {
                current_name = Some(parts[0].trim().to_string());
                current_expr = parts[1].trim().to_string();
                current_expr.push('\n');
            }
            continue;
        }

        if in_liveness && current_name.is_some() {
            if trimmed.is_empty() {
                if let Some(name) = current_name.take() {
                    properties.push(LivenessProperty {
                        name,
                        expr: current_expr.trim().to_string(),
                    });
                    current_expr.clear();
                }
            } else {
                current_expr.push_str(trimmed);
                current_expr.push('\n');
            }
        }
    }

    if let Some(name) = current_name {
        properties.push(LivenessProperty {
            name,
            expr: current_expr.trim().to_string(),
        });
    }

    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_reference_order_tla() {
        let tla = include_str!("../../../../test-fixtures/specs/order.tla");
        let sm = extract_state_machine(tla).expect("should extract without error");

        assert_eq!(sm.module_name, "Order");

        // States
        assert!(sm.states.contains(&"Draft".to_string()));
        assert!(sm.states.contains(&"Submitted".to_string()));
        assert!(sm.states.contains(&"Shipped".to_string()));
        assert!(sm.states.contains(&"Refunded".to_string()));
        assert_eq!(sm.states.len(), 10);

        // Constants
        assert!(sm.constants.contains(&"MAX_ITEMS".to_string()));
        assert!(sm.constants.contains(&"MAX_ORDER_TOTAL".to_string()));

        // Variables
        assert!(sm.variables.contains(&"status".to_string()));
        assert!(sm.variables.contains(&"items".to_string()));
        assert!(sm.variables.contains(&"total".to_string()));

        // Transitions — should find the main actions
        let transition_names: Vec<&str> = sm.transitions.iter().map(|t| t.name.as_str()).collect();
        assert!(
            transition_names.contains(&"SubmitOrder"),
            "should have SubmitOrder, got: {transition_names:?}"
        );
        assert!(transition_names.contains(&"ConfirmOrder"));
        assert!(transition_names.contains(&"ShipOrder"));
        assert!(transition_names.contains(&"DeliverOrder"));
        assert!(transition_names.contains(&"CancelOrder"), "got: {transition_names:?}");
        assert!(transition_names.contains(&"InitiateReturn"));

        // Verify SubmitOrder transition details
        let submit = sm.transitions.iter().find(|t| t.name == "SubmitOrder").unwrap();
        assert_eq!(submit.to_state, Some("Submitted".to_string()));

        // Invariants
        assert!(!sm.invariants.is_empty(), "should have invariants");
        let inv_names: Vec<&str> = sm.invariants.iter().map(|i| i.name.as_str()).collect();
        assert!(
            inv_names.contains(&"TypeInvariant"),
            "should have TypeInvariant, got: {inv_names:?}"
        );
        assert!(inv_names.contains(&"ShipRequiresPayment"));

        // Liveness
        assert!(!sm.liveness_properties.is_empty(), "should have liveness properties");
        let live_names: Vec<&str> = sm.liveness_properties.iter().map(|l| l.name.as_str()).collect();
        assert!(
            live_names.contains(&"SubmittedProgress"),
            "should have SubmittedProgress, got: {live_names:?}"
        );
    }

    #[test]
    fn test_extract_module_name() {
        let source = "---- MODULE TestModule ----\n\\* Some comment\n====";
        let name = extract_module_name(source).unwrap();
        assert_eq!(name, "TestModule");
    }

    #[test]
    fn test_extract_states_from_set() {
        let source = r#"
States == {"Active", "Inactive", "Deleted"}
"#;
        let states = extract_states(source).unwrap();
        assert_eq!(states, vec!["Active", "Inactive", "Deleted"]);
    }
}

#[cfg(test)]
mod debug {
    use super::*;
    #[test]
    fn debug_cancel() {
        let tla = include_str!("../../../../test-fixtures/specs/order.tla");
        let sm = extract_state_machine(tla).unwrap();
        for t in &sm.transitions {
            if t.name.contains("Cancel") || t.name.contains("Initiate") {
                eprintln!("{}: from={:?} to={:?} has_params={}", t.name, t.from_states, t.to_state, t.has_parameters);
            }
        }
    }
}
