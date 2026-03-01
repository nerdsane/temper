//! Spec metadata extraction for Cedar policy evaluation.
//!
//! Extracts flat, Cedar-compatible metadata from parsed Automaton specs.
//! Used by platform presets to enforce structural rules on specs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::types::Automaton;

/// Flat metadata extracted from an Automaton spec, suitable for Cedar context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecMetadata {
    /// Entity type name.
    pub entity_name: String,
    /// All valid status states.
    pub states: Vec<String>,
    /// Initial status value.
    pub initial_state: String,
    /// Number of states.
    pub state_count: i64,
    /// All action names.
    pub action_names: Vec<String>,
    /// Number of actions.
    pub action_count: i64,
    /// All invariant names.
    pub invariant_names: Vec<String>,
    /// Number of invariants.
    pub invariant_count: i64,
    /// Transition edges as "FromState->ToState" strings.
    pub transition_edges: Vec<String>,
    /// Target states of transitions that have guard conditions.
    pub guarded_target_states: Vec<String>,
    /// Human-readable guard summaries.
    pub guard_summaries: Vec<String>,
    /// Integration names.
    pub integration_names: Vec<String>,
    /// Terminal states (states with no outgoing transitions).
    pub terminal_states: Vec<String>,
    /// Whether any liveness properties are defined.
    pub has_liveness: bool,
}

impl SpecMetadata {
    /// Convert to a flat BTreeMap for Cedar context injection.
    pub fn to_flat_map(&self) -> BTreeMap<String, serde_json::Value> {
        let mut map = BTreeMap::new();
        map.insert("entity_name".into(), serde_json::json!(self.entity_name));
        map.insert("states".into(), serde_json::json!(self.states));
        map.insert(
            "initial_state".into(),
            serde_json::json!(self.initial_state),
        );
        map.insert("state_count".into(), serde_json::json!(self.state_count));
        map.insert("action_names".into(), serde_json::json!(self.action_names));
        map.insert("action_count".into(), serde_json::json!(self.action_count));
        map.insert(
            "invariant_names".into(),
            serde_json::json!(self.invariant_names),
        );
        map.insert(
            "invariant_count".into(),
            serde_json::json!(self.invariant_count),
        );
        map.insert(
            "transition_edges".into(),
            serde_json::json!(self.transition_edges),
        );
        map.insert(
            "guarded_target_states".into(),
            serde_json::json!(self.guarded_target_states),
        );
        map.insert(
            "guard_summaries".into(),
            serde_json::json!(self.guard_summaries),
        );
        map.insert(
            "integration_names".into(),
            serde_json::json!(self.integration_names),
        );
        map.insert(
            "terminal_states".into(),
            serde_json::json!(self.terminal_states),
        );
        map.insert("has_liveness".into(), serde_json::json!(self.has_liveness));
        map
    }
}

impl Automaton {
    /// Extract flat metadata suitable for Cedar policy evaluation.
    pub fn extract_metadata(&self) -> SpecMetadata {
        let states = self.automaton.states.clone();
        let initial_state = self.automaton.initial.clone();

        let action_names: Vec<String> = self.actions.iter().map(|a| a.name.clone()).collect();
        let invariant_names: Vec<String> = self.invariants.iter().map(|i| i.name.clone()).collect();
        let integration_names: Vec<String> =
            self.integrations.iter().map(|i| i.name.clone()).collect();

        // Build transition edges and find guarded target states
        let mut transition_edges = Vec::new();
        let mut guarded_target_states = Vec::new();
        let mut guard_summaries = Vec::new();
        let mut from_states = std::collections::BTreeSet::new();

        for action in &self.actions {
            if let Some(ref to) = action.to {
                for from in &action.from {
                    transition_edges.push(format!("{}->{}", from, to));
                    from_states.insert(from.clone());
                }
                // If this action has guards, its target is a guarded target state
                if !action.guard.is_empty() {
                    if !guarded_target_states.contains(to) {
                        guarded_target_states.push(to.clone());
                    }
                    for guard in &action.guard {
                        guard_summaries.push(format!("{}: {:?}", action.name, guard));
                    }
                }
            } else {
                // Self-loop (no `to` means status unchanged)
                for from in &action.from {
                    from_states.insert(from.clone());
                }
            }
        }

        // Terminal states: states that never appear in any action's `from`
        let terminal_states: Vec<String> = states
            .iter()
            .filter(|s| !from_states.contains(*s))
            .cloned()
            .collect();

        SpecMetadata {
            entity_name: self.automaton.name.clone(),
            states: states.clone(),
            initial_state,
            state_count: states.len() as i64,
            action_names,
            action_count: self.actions.len() as i64,
            invariant_names,
            invariant_count: self.invariants.len() as i64,
            transition_edges,
            guarded_target_states,
            guard_summaries,
            integration_names,
            terminal_states,
            has_liveness: !self.liveness.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::automaton::parse_automaton;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_order_metadata_extraction() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let metadata = automaton.extract_metadata();

        assert_eq!(metadata.entity_name, "Order");
        assert!(metadata.states.contains(&"Draft".to_string()));
        assert_eq!(metadata.initial_state, "Draft");
        assert!(metadata.state_count >= 4);
        assert!(!metadata.action_names.is_empty());
        assert_eq!(metadata.action_count, metadata.action_names.len() as i64);
    }

    #[test]
    fn test_terminal_states() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let metadata = automaton.extract_metadata();

        // Terminal states should be states with no outgoing transitions
        for ts in &metadata.terminal_states {
            assert!(
                metadata.states.contains(ts),
                "Terminal state {ts} not in states"
            );
        }
    }

    #[test]
    fn test_transition_edges_format() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let metadata = automaton.extract_metadata();

        for edge in &metadata.transition_edges {
            assert!(edge.contains("->"), "Edge should contain '->': {edge}");
        }
    }

    #[test]
    fn test_to_flat_map() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let metadata = automaton.extract_metadata();
        let map = metadata.to_flat_map();

        assert!(map.contains_key("entity_name"));
        assert!(map.contains_key("states"));
        assert!(map.contains_key("state_count"));
        assert!(map.contains_key("has_liveness"));
        assert_eq!(map["state_count"], serde_json::json!(metadata.state_count));
    }

    #[test]
    fn test_empty_spec_metadata() {
        let spec = r#"
[automaton]
name = "Empty"
states = ["A"]
initial = "A"
"#;
        let automaton = parse_automaton(spec).unwrap();
        let metadata = automaton.extract_metadata();

        assert_eq!(metadata.entity_name, "Empty");
        assert_eq!(metadata.state_count, 1);
        assert_eq!(metadata.action_count, 0);
        assert_eq!(metadata.invariant_count, 0);
        assert!(!metadata.has_liveness);
        // All states are terminal since no actions
        assert_eq!(metadata.terminal_states, vec!["A".to_string()]);
    }
}
