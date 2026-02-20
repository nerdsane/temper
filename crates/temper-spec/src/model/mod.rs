use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::automaton;
use crate::csdl;
use crate::tlaplus;

/// Identifies whether a spec source is IOA TOML (primary) or TLA+ (legacy).
#[derive(Debug, Clone)]
pub enum SpecSource {
    /// I/O Automaton TOML source (primary format).
    Ioa(String),
    /// TLA+ source (legacy format).
    Tla(String),
}

/// The unified specification model that links CSDL + specification sources (IOA/TLA+).
/// This is what codegen consumes to produce Rust actors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecModel {
    /// The CSDL document (data model).
    pub csdl: csdl::CsdlDocument,
    /// State machines keyed by entity type name (from IOA or TLA+ sources).
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

/// Build a unified SpecModel from a CSDL document and specification sources.
///
/// `tla_sources` maps entity type name → TLA+ source text (legacy API).
/// For mixed IOA + TLA+ sources, use [`build_spec_model_mixed`].
pub fn build_spec_model(
    csdl: csdl::CsdlDocument,
    tla_sources: HashMap<String, String>,
) -> SpecModel {
    let sources: HashMap<String, SpecSource> = tla_sources
        .into_iter()
        .map(|(k, v)| (k, SpecSource::Tla(v)))
        .collect();
    build_spec_model_mixed(csdl, sources)
}

/// Build a unified SpecModel from a CSDL document and mixed specification sources.
///
/// `sources` maps entity type name → [`SpecSource`] (either IOA or TLA+).
/// IOA sources go through `parse_automaton()` → `to_state_machine()`.
/// TLA+ sources go through `extract_state_machine()`.
/// Both produce the same `StateMachine` for the codegen pipeline.
pub fn build_spec_model_mixed(
    csdl: csdl::CsdlDocument,
    sources: HashMap<String, SpecSource>,
) -> SpecModel {
    let mut state_machines = HashMap::new();
    let mut validation = ValidationResult::default();

    // Parse each specification source
    for (entity_name, source) in &sources {
        match source {
            SpecSource::Tla(tla_text) => match tlaplus::extract_state_machine(tla_text) {
                Ok(sm) => {
                    state_machines.insert(entity_name.clone(), sm);
                }
                Err(e) => {
                    validation.errors.push(format!(
                        "Failed to extract state machine for {entity_name} (TLA+): {e}"
                    ));
                }
            },
            SpecSource::Ioa(ioa_text) => match automaton::parse_automaton(ioa_text) {
                Ok(aut) => {
                    let sm = automaton::to_state_machine(&aut);
                    state_machines.insert(entity_name.clone(), sm);
                }
                Err(e) => {
                    validation.errors.push(format!(
                        "Failed to parse IOA automaton for {entity_name}: {e}"
                    ));
                }
            },
        }
    }

    // Cross-validate CSDL annotations against specification state machines
    for schema in &csdl.schemas {
        for entity_type in &schema.entity_types {
            if let Some(csdl_states) = entity_type.state_machine_states() {
                if let Some(sm) = state_machines.get(&entity_type.name) {
                    // Verify all CSDL-declared states exist in spec
                    for state in &csdl_states {
                        if !sm.states.contains(state) {
                            validation.errors.push(format!(
                                "{}: CSDL declares state '{}' but specification does not contain it",
                                entity_type.name, state
                            ));
                        }
                    }
                    // Verify all spec states are in CSDL
                    for state in &sm.states {
                        if !csdl_states.contains(state) {
                            validation.warnings.push(format!(
                                "{}: specification has state '{}' not declared in CSDL annotations",
                                entity_type.name, state
                            ));
                        }
                    }
                } else if entity_type.tla_spec_path().is_some() {
                    validation.warnings.push(format!(
                        "{}: has TlaSpec annotation but no specification source was provided",
                        entity_type.name
                    ));
                }
            }
        }

        // Validate action valid-from states against specification transitions
        for action in &schema.actions {
            if let Some(from_states) = action.valid_from_states() {
                if let Some(binding_type) = action.binding_type() {
                    let entity_name = binding_type.rsplit('.').next().unwrap_or(binding_type);
                    if let Some(sm) = state_machines.get(entity_name) {
                        for state in &from_states {
                            if !sm.states.contains(state) {
                                validation.errors.push(format!(
                                    "Action {}: ValidFromStates contains '{}' which is not in {}'s specification states",
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

    #[test]
    fn test_build_spec_model_from_ioa() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        let mut sources = HashMap::new();
        sources.insert("Order".to_string(), SpecSource::Ioa(order_ioa.to_string()));

        let spec = build_spec_model_mixed(csdl, sources);

        // Should be valid (no errors)
        assert!(
            spec.validation.is_valid(),
            "validation errors: {:?}",
            spec.validation.errors
        );

        // Should have the Order state machine from IOA
        assert!(spec.state_machines.contains_key("Order"));

        let order_sm = &spec.state_machines["Order"];
        assert!(!order_sm.states.is_empty());
        assert!(!order_sm.transitions.is_empty());
    }

    #[test]
    fn test_ioa_takes_precedence_over_tla() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
        let order_tla = include_str!("../../../../test-fixtures/specs/order.tla");

        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        // Build with IOA only
        let mut ioa_sources = HashMap::new();
        ioa_sources.insert("Order".to_string(), SpecSource::Ioa(order_ioa.to_string()));
        let ioa_spec = build_spec_model_mixed(csdl.clone(), ioa_sources);

        // Build with TLA+ only
        let mut tla_sources = HashMap::new();
        tla_sources.insert("Order".to_string(), SpecSource::Tla(order_tla.to_string()));
        let tla_spec = build_spec_model_mixed(csdl, tla_sources);

        // Both should produce valid specs
        assert!(ioa_spec.validation.is_valid());
        assert!(tla_spec.validation.is_valid());

        // Both should have Order state machine
        assert!(ioa_spec.state_machines.contains_key("Order"));
        assert!(tla_spec.state_machines.contains_key("Order"));
    }
}
