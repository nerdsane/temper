use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::automaton::{self, Automaton};
use crate::csdl;

/// The unified specification model that links CSDL (data) to Automaton (behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecModel {
    /// The CSDL document (data model).
    pub csdl: csdl::CsdlDocument,
    /// Parsed automata keyed by entity type name.
    pub automata: HashMap<String, Automaton>,
    /// Validation results from linking CSDL annotations to Automaton states.
    pub validation: ValidationResult,
}

/// Result of linking CSDL annotations to Automaton states.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationResult {
    /// Validation failures that should block linking.
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

/// Build a [`SpecModel`] from a CSDL document and I/O Automaton TOML sources.
///
/// `ioa_sources` maps entity type name → IOA TOML source text. Each source is
/// parsed once to [`Automaton`]. CSDL `TlaSpec` annotations are ignored XML
/// (ADR-0169 / ARN-383) and do not require a `.tla` file.
pub fn build_spec_model(
    csdl: csdl::CsdlDocument,
    ioa_sources: HashMap<String, String>,
) -> SpecModel {
    let mut validation = ValidationResult::default();
    let automata = parse_automata(&ioa_sources, &mut validation);
    validate_csdl_links(&csdl, &automata, &mut validation);

    SpecModel {
        csdl,
        automata,
        validation,
    }
}

fn parse_automata(
    sources: &HashMap<String, String>,
    validation: &mut ValidationResult,
) -> HashMap<String, Automaton> {
    let mut automata = HashMap::new();

    for (entity_name, ioa_text) in sources {
        match automaton::parse_automaton(ioa_text) {
            Ok(parsed) => {
                automata.insert(entity_name.clone(), parsed);
            }
            Err(error) => validation.errors.push(format!(
                "Failed to parse IOA automaton for {entity_name}: {error}"
            )),
        }
    }

    automata
}

fn validate_csdl_links(
    csdl: &csdl::CsdlDocument,
    automata: &HashMap<String, Automaton>,
    validation: &mut ValidationResult,
) {
    for schema in &csdl.schemas {
        validate_entity_states(schema, automata, validation);
        validate_action_bindings(schema, automata, validation);
    }
}

fn validate_entity_states(
    schema: &csdl::Schema,
    automata: &HashMap<String, Automaton>,
    validation: &mut ValidationResult,
) {
    for entity_type in &schema.entity_types {
        let Some(csdl_states) = entity_type.state_machine_states() else {
            continue;
        };

        if let Some(automaton) = automata.get(&entity_type.name) {
            record_missing_csdl_states(entity_type, &csdl_states, automaton, validation);
            record_missing_spec_states(entity_type, &csdl_states, automaton, validation);
        } else if entity_type.tla_spec_path().is_some() {
            validation.warnings.push(format!(
                "{}: has TlaSpec annotation (ignored; Automaton is the behavior IR) and no I/O Automaton source was provided",
                entity_type.name
            ));
        }
    }
}

fn record_missing_csdl_states(
    entity_type: &csdl::EntityType,
    csdl_states: &[String],
    automaton: &Automaton,
    validation: &mut ValidationResult,
) {
    for state in csdl_states {
        if !automaton.automaton.states.contains(state) {
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
    automaton: &Automaton,
    validation: &mut ValidationResult,
) {
    for state in &automaton.automaton.states {
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
    automata: &HashMap<String, Automaton>,
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
        let Some(automaton) = automata.get(entity_name) else {
            continue;
        };

        for state in &from_states {
            if !automaton.automaton.states.contains(state) {
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

        assert!(
            spec.validation.is_valid(),
            "validation errors: {:?}",
            spec.validation.errors
        );

        assert!(spec.automata.contains_key("Order"));

        let order = &spec.automata["Order"];
        assert_eq!(order.automaton.states.len(), 10);
        assert!(!order.actions.is_empty());
        assert!(!order.invariants.is_empty());
    }

    #[test]
    fn test_tla_spec_annotation_does_not_require_tla_file() {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");

        let spec = build_spec_model(csdl, HashMap::new());

        assert!(spec.validation.is_valid());
        assert!(
            spec.validation
                .warnings
                .iter()
                .any(|w| w.contains("TlaSpec") && w.contains("Order")),
            "TlaSpec without IOA should warn, got: {:?}",
            spec.validation.warnings
        );
    }
}
