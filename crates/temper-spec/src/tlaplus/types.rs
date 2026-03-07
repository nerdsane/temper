use serde::{Deserialize, Serialize};

/// Extracted state machine structure from a TLA+ specification.
/// This is NOT a full TLA+ parser — it extracts the structured elements
/// that Temper's codegen needs: states, transitions, guards, invariants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMachine {
    /// Module name from the TLA+ spec.
    pub module_name: String,
    /// All declared state values.
    pub states: Vec<String>,
    /// All declared transitions.
    pub transitions: Vec<Transition>,
    /// Safety invariants (properties that must always hold).
    pub invariants: Vec<Invariant>,
    /// Liveness properties (something good eventually happens).
    pub liveness_properties: Vec<LivenessProperty>,
    /// Constants declared in the spec.
    pub constants: Vec<String>,
    /// Variables declared in the spec.
    pub variables: Vec<String>,
}

/// A state machine transition (action).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    /// The action name (e.g., "SubmitOrder").
    pub name: String,
    /// States this transition can be taken from (extracted from guards).
    pub from_states: Vec<String>,
    /// The target state after this transition (if deterministic).
    pub to_state: Option<String>,
    /// Raw guard expression (the TLA+ precondition).
    pub guard_expr: String,
    /// Whether this transition has parameters.
    pub has_parameters: bool,
    /// Raw effect expression (what changes).
    pub effect_expr: String,
}

/// A safety invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invariant {
    /// Invariant name (e.g., "ShipRequiresPayment").
    pub name: String,
    /// Raw TLA+ expression.
    pub expr: String,
}

/// A liveness property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivenessProperty {
    /// Property name.
    pub name: String,
    /// Raw TLA+ expression.
    pub expr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_serde_roundtrip() {
        let sm = StateMachine {
            module_name: "OrderSpec".into(),
            states: vec!["Draft".into(), "Active".into()],
            transitions: vec![Transition {
                name: "Submit".into(),
                from_states: vec!["Draft".into()],
                to_state: Some("Active".into()),
                guard_expr: "status = \"Draft\"".into(),
                has_parameters: false,
                effect_expr: "status' = \"Active\"".into(),
            }],
            invariants: vec![Invariant {
                name: "TypeOK".into(),
                expr: "status \\in States".into(),
            }],
            liveness_properties: vec![LivenessProperty {
                name: "Progress".into(),
                expr: "<>(status = \"Active\")".into(),
            }],
            constants: vec!["MaxItems".into()],
            variables: vec!["status".into()],
        };
        let json = serde_json::to_string(&sm).unwrap();
        let back: StateMachine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.module_name, "OrderSpec");
        assert_eq!(back.states.len(), 2);
        assert_eq!(back.transitions.len(), 1);
        assert_eq!(back.transitions[0].name, "Submit");
        assert_eq!(back.transitions[0].to_state, Some("Active".into()));
        assert_eq!(back.invariants.len(), 1);
        assert_eq!(back.liveness_properties.len(), 1);
        assert_eq!(back.constants, vec!["MaxItems"]);
        assert_eq!(back.variables, vec!["status"]);
    }

    #[test]
    fn transition_optional_to_state() {
        let t = Transition {
            name: "SelfLoop".into(),
            from_states: vec!["A".into()],
            to_state: None,
            guard_expr: "TRUE".into(),
            has_parameters: true,
            effect_expr: "UNCHANGED".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: Transition = serde_json::from_str(&json).unwrap();
        assert!(back.to_state.is_none());
        assert!(back.has_parameters);
    }
}
