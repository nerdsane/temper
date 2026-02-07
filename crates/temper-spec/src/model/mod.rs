use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::csdl;
use crate::tlaplus;

/// The unified specification model that links CSDL + TLA+.
/// This is what codegen consumes to produce Rust actors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecModel {
    /// The CSDL document (data model).
    pub csdl: csdl::CsdlDocument,
    /// TLA+ state machines keyed by entity type name.
    pub state_machines: HashMap<String, tlaplus::StateMachine>,
    /// Validation results from linking.
    pub validation: ValidationResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationResult {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationResult {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Build a unified SpecModel from a CSDL document and TLA+ sources.
///
/// `tla_sources` maps entity type name → TLA+ source text.
pub fn build_spec_model(
    csdl: csdl::CsdlDocument,
    tla_sources: HashMap<String, String>,
) -> SpecModel {
    let mut state_machines = HashMap::new();
    let mut validation = ValidationResult::default();

    // Parse each TLA+ source
    for (entity_name, source) in &tla_sources {
        match tlaplus::extract_state_machine(source) {
            Ok(sm) => {
                state_machines.insert(entity_name.clone(), sm);
            }
            Err(e) => {
                validation.errors.push(format!(
                    "Failed to extract state machine for {entity_name}: {e}"
                ));
            }
        }
    }

    // Cross-validate CSDL annotations against TLA+ specs
    for schema in &csdl.schemas {
        for entity_type in &schema.entity_types {
            if let Some(csdl_states) = entity_type.state_machine_states() {
                if let Some(sm) = state_machines.get(&entity_type.name) {
                    // Verify all CSDL-declared states exist in TLA+
                    for state in &csdl_states {
                        if !sm.states.contains(state) {
                            validation.errors.push(format!(
                                "{}: CSDL declares state '{}' but TLA+ spec does not contain it",
                                entity_type.name, state
                            ));
                        }
                    }
                    // Verify all TLA+ states are in CSDL
                    for state in &sm.states {
                        if !csdl_states.contains(state) {
                            validation.warnings.push(format!(
                                "{}: TLA+ has state '{}' not declared in CSDL annotations",
                                entity_type.name, state
                            ));
                        }
                    }
                } else if entity_type.tla_spec_path().is_some() {
                    validation.warnings.push(format!(
                        "{}: has TlaSpec annotation but no TLA+ source was provided",
                        entity_type.name
                    ));
                }
            }
        }

        // Validate action valid-from states against TLA+ transitions
        for action in &schema.actions {
            if let Some(from_states) = action.valid_from_states() {
                if let Some(binding_type) = action.binding_type() {
                    let entity_name = binding_type.rsplit('.').next().unwrap_or(binding_type);
                    if let Some(sm) = state_machines.get(entity_name) {
                        for state in &from_states {
                            if !sm.states.contains(state) {
                                validation.errors.push(format!(
                                    "Action {}: ValidFromStates contains '{}' which is not in {}'s TLA+ states",
                                    action.name, state, entity_name
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    SpecModel {
        csdl,
        state_machines,
        validation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csdl::parse_csdl;

    #[test]
    fn test_build_spec_model_from_reference() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_tla = include_str!("../../../../test-fixtures/specs/order.tla");

        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        let mut tla_sources = HashMap::new();
        tla_sources.insert("Order".to_string(), order_tla.to_string());

        let spec = build_spec_model(csdl, tla_sources);

        // Should be valid (no errors)
        assert!(
            spec.validation.is_valid(),
            "validation errors: {:?}",
            spec.validation.errors
        );

        // Should have the Order state machine
        assert!(spec.state_machines.contains_key("Order"));

        let order_sm = &spec.state_machines["Order"];
        assert_eq!(order_sm.states.len(), 10);
        assert!(!order_sm.transitions.is_empty());
        assert!(!order_sm.invariants.is_empty());
    }
}
