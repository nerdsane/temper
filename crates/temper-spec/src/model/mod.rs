use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::automaton;
use crate::csdl;
use crate::state_machine;

/// The unified specification model that links CSDL + IOA specification sources.
/// This is what codegen consumes to produce Rust actors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecModel {
    /// The CSDL document (data model).
    pub csdl: csdl::CsdlDocument,
    /// State machines keyed by entity type name (from IOA sources).
    pub state_machines: HashMap<String, state_machine::StateMachine>,
    /// Validation results from linking.
    pub validation: ValidationResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationResult {
    /// Validation failures that should block downstream codegen or linking.
    pub errors: Vec<String>,
    /// Non-blocking mismatches or gaps detected during spec linking.
    pub warnings: Vec<String>,
}

impl ValidationResult {
    /// Returns true when the linked specification contains no validation errors.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Build a unified SpecModel from a CSDL document and IOA specification sources.
///
/// `ioa_sources` maps entity type name → IOA TOML source text. Each source
/// goes through `parse_automaton()` → `to_state_machine()` to produce the
/// `StateMachine` IR for the codegen pipeline.
pub fn build_spec_model(
    csdl: csdl::CsdlDocument,
    ioa_sources: HashMap<String, String>,
) -> SpecModel {
    let mut validation = ValidationResult::default();
    let state_machines = parse_state_machines(&ioa_sources, &mut validation);
    validate_csdl_links(&csdl, &state_machines, &mut validation);

    SpecModel {
        csdl,
        state_machines,
        validation,
    }
}

fn parse_state_machines(
    ioa_sources: &HashMap<String, String>,
    validation: &mut ValidationResult,
) -> HashMap<String, state_machine::StateMachine> {
    let mut state_machines = HashMap::new();

    for (entity_name, ioa_text) in ioa_sources {
        match automaton::parse_automaton(ioa_text) {
            Ok(automaton) => {
                state_machines.insert(entity_name.clone(), automaton::to_state_machine(&automaton));
            }
            Err(error) => validation.errors.push(format!(
                "Failed to parse IOA automaton for {entity_name}: {error}"
            )),
        }
    }

    state_machines
}

fn validate_csdl_links(
    csdl: &csdl::CsdlDocument,
    state_machines: &HashMap<String, state_machine::StateMachine>,
    validation: &mut ValidationResult,
) {
    for schema in &csdl.schemas {
        validate_entity_states(schema, state_machines, validation);
        validate_action_bindings(schema, state_machines, validation);
    }
}

fn validate_entity_states(
    schema: &csdl::Schema,
    state_machines: &HashMap<String, state_machine::StateMachine>,
    validation: &mut ValidationResult,
) {
    for entity_type in &schema.entity_types {
        let Some(csdl_states) = entity_type.state_machine_states() else {
            continue;
        };

        if let Some(state_machine) = state_machines.get(&entity_type.name) {
            record_missing_csdl_states(entity_type, &csdl_states, state_machine, validation);
            record_missing_spec_states(entity_type, &csdl_states, state_machine, validation);
        } else if entity_type.tla_spec_path().is_some() {
            validation.warnings.push(format!(
                "{}: has TlaSpec annotation but no specification source was provided",
                entity_type.name
            ));
        }
    }
}

fn record_missing_csdl_states(
    entity_type: &csdl::EntityType,
    csdl_states: &[String],
    state_machine: &state_machine::StateMachine,
    validation: &mut ValidationResult,
) {
    for state in csdl_states {
        if !state_machine.states.contains(state) {
            validation.errors.push(format!(
                "{}: CSDL declares state '{}' but specification does not contain it",
                entity_type.name, state
            ));
        }
    }
}

fn record_missing_spec_states(
    entity_type: &csdl::EntityType,
    csdl_states: &[String],
    state_machine: &state_machine::StateMachine,
    validation: &mut ValidationResult,
) {
    for state in &state_machine.states {
        if !csdl_states.contains(state) {
            validation.warnings.push(format!(
                "{}: specification has state '{}' not declared in CSDL annotations",
                entity_type.name, state
            ));
        }
    }
}

fn validate_action_bindings(
    schema: &csdl::Schema,
    state_machines: &HashMap<String, state_machine::StateMachine>,
    validation: &mut ValidationResult,
) {
    for action in &schema.actions {
        let Some(from_states) = action.valid_from_states() else {
            continue;
        };
        let Some(binding_type) = action.binding_type() else {
            continue;
        };

        let entity_name = binding_type.rsplit('.').next().unwrap_or(binding_type);
        let Some(state_machine) = state_machines.get(entity_name) else {
            continue;
        };

        for state in &from_states {
            if !state_machine.states.contains(state) {
                validation.errors.push(format!(
                    "Action {}: ValidFromStates contains '{}' which is not in {}'s specification states",
                    action.name, state, entity_name
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csdl::parse_csdl;

    #[test]
    fn test_build_spec_model_from_ioa() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        let mut sources = HashMap::new();
        sources.insert("Order".to_string(), order_ioa.to_string());

        let spec = build_spec_model(csdl, sources);

        // Should be valid (no errors)
        assert!(
            spec.validation.is_valid(),
            "validation errors: {:?}",
            spec.validation.errors
        );

        // Should have the Order state machine from IOA
        assert!(spec.state_machines.contains_key("Order"));

        let order_sm = &spec.state_machines["Order"];
        assert_eq!(order_sm.states.len(), 10);
        assert!(!order_sm.transitions.is_empty());
        assert!(!order_sm.invariants.is_empty());
    }

    #[test]
    fn test_build_spec_model_reports_parse_errors() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        let mut sources = HashMap::new();
        sources.insert("Order".to_string(), "not valid toml [[".to_string());

        let spec = build_spec_model(csdl, sources);

        assert!(!spec.validation.is_valid());
        assert!(
            spec.validation.errors[0].contains("Failed to parse IOA automaton for Order"),
            "unexpected error: {:?}",
            spec.validation.errors
        );
    }
}
