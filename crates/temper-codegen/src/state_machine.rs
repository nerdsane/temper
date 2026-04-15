//! Generate state machine enums and transition tables from TLA+ specs.

use temper_spec::tlaplus::StateMachine;

/// Generate the state enum and transition table for an entity.
pub fn generate_state_machine(entity_name: &str, sm: &StateMachine) -> String {
    let mut out = String::new();

    // State enum
    out.push_str(&format!(
        "/// State machine states for {} (generated from TLA+ spec: {}).\n",
        entity_name, sm.module_name
    ));
    out.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]\n");
    out.push_str(&format!("pub enum {}Status {{\n", entity_name));
    for state in &sm.states {
        out.push_str(&format!("    {},\n", state));
    }
    out.push_str("}\n\n");

    // Display impl
    out.push_str(&format!(
        "impl std::fmt::Display for {}Status {{\n",
        entity_name
    ));
    out.push_str("    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n");
    out.push_str("        match self {\n");
    for state in &sm.states {
        out.push_str(&format!(
            "            Self::{} => write!(f, \"{}\"),\n",
            state, state
        ));
    }
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // Transition table: which transitions are valid from which states
    out.push_str(&format!(
        "/// Transition table for {} state machine.\n",
        entity_name
    ));
    out.push_str(&format!("pub struct {}Transitions;\n\n", entity_name));
    out.push_str(&format!("impl {}Transitions {{\n", entity_name));

    // can_transition method
    out.push_str("    /// Check if a transition is valid from the current state.\n");
    out.push_str(&format!(
        "    pub fn can_transition(current: {}Status, action: &str) -> bool {{\n",
        entity_name
    ));
    out.push_str("        match (current, action) {\n");

    for transition in &sm.transitions {
        if transition.from_states.is_empty() {
            // If no explicit from-states, allow from any state
            out.push_str(&format!(
                "            (_, \"{}\") => true,\n",
                transition.name
            ));
        } else {
            for from in &transition.from_states {
                out.push_str(&format!(
                    "            ({}Status::{}, \"{}\") => true,\n",
                    entity_name, from, transition.name
                ));
            }
        }
    }

    out.push_str("            _ => false,\n");
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    // target_state method
    out.push_str("    /// Get the target state for a transition.\n");
    out.push_str(&format!(
        "    pub fn target_state(action: &str) -> Option<{}Status> {{\n",
        entity_name
    ));
    out.push_str("        match action {\n");

    for transition in &sm.transitions {
        if let Some(ref to) = transition.to_state {
            out.push_str(&format!(
                "            \"{}\" => Some({}Status::{}),\n",
                transition.name, entity_name, to
            ));
        }
    }

    out.push_str("            _ => None,\n");
    out.push_str("        }\n");
    out.push_str("    }\n");

    out.push_str("}\n\n");

    // Generate invariant check functions
    if !sm.invariants.is_empty() {
        out.push_str(&format!(
            "/// Invariant checks for {} (from TLA+ spec).\n",
            entity_name
        ));
        out.push_str(&format!("pub struct {}Invariants;\n\n", entity_name));
        out.push_str(&format!("impl {}Invariants {{\n", entity_name));
        out.push_str("    /// Names of all invariants that must hold.\n");
        out.push_str("    pub fn invariant_names() -> &'static [&'static str] {\n");
        out.push_str("        &[\n");
        for inv in &sm.invariants {
            out.push_str(&format!("            \"{}\",\n", inv.name));
        }
        out.push_str("        ]\n");
        out.push_str("    }\n");
        out.push_str("}\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::tlaplus::{Invariant, Transition};

    fn test_sm() -> StateMachine {
        StateMachine {
            module_name: "OrderSpec".to_string(),
            states: vec![
                "Draft".to_string(),
                "Submitted".to_string(),
                "Done".to_string(),
            ],
            transitions: vec![
                Transition {
                    name: "Submit".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Submitted".to_string()),
                    guard_expr: String::new(),
                    effect_expr: String::new(),
                    has_parameters: false,
                },
                Transition {
                    name: "Complete".to_string(),
                    from_states: vec!["Submitted".to_string()],
                    to_state: Some("Done".to_string()),
                    guard_expr: String::new(),
                    effect_expr: String::new(),
                    has_parameters: false,
                },
            ],
            invariants: vec![Invariant {
                name: "DraftBeforeSubmit".to_string(),
                expr: String::new(),
            }],
            liveness_properties: vec![],
            constants: vec![],
            variables: vec![],
        }
    }

    #[test]
    fn generates_state_enum() {
        let code = generate_state_machine("Order", &test_sm());
        assert!(code.contains("pub enum OrderStatus {"));
        assert!(code.contains("    Draft,"));
        assert!(code.contains("    Submitted,"));
        assert!(code.contains("    Done,"));
    }

    #[test]
    fn generates_display_impl() {
        let code = generate_state_machine("Order", &test_sm());
        assert!(code.contains("impl std::fmt::Display for OrderStatus"));
        assert!(code.contains("Self::Draft => write!(f, \"Draft\")"));
    }

    #[test]
    fn generates_transition_table() {
        let code = generate_state_machine("Order", &test_sm());
        assert!(code.contains("pub struct OrderTransitions;"));
        assert!(code.contains("pub fn can_transition(current: OrderStatus, action: &str) -> bool"));
        assert!(code.contains("(OrderStatus::Draft, \"Submit\") => true,"));
        assert!(code.contains("(OrderStatus::Submitted, \"Complete\") => true,"));
        assert!(code.contains("_ => false,"));
    }

    #[test]
    fn generates_target_state() {
        let code = generate_state_machine("Order", &test_sm());
        assert!(code.contains("pub fn target_state(action: &str) -> Option<OrderStatus>"));
        assert!(code.contains("\"Submit\" => Some(OrderStatus::Submitted),"));
        assert!(code.contains("\"Complete\" => Some(OrderStatus::Done),"));
    }

    #[test]
    fn generates_invariant_names() {
        let code = generate_state_machine("Order", &test_sm());
        assert!(code.contains("pub struct OrderInvariants;"));
        assert!(code.contains("pub fn invariant_names()"));
        assert!(code.contains("\"DraftBeforeSubmit\""));
    }

    #[test]
    fn no_invariants_skips_section() {
        let mut sm = test_sm();
        sm.invariants.clear();
        let code = generate_state_machine("Order", &sm);
        assert!(!code.contains("OrderInvariants"));
    }
}
