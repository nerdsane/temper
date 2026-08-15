use std::collections::BTreeMap;

use super::{Automaton, BundleLintFinding};

pub(super) fn lint_reference_targets(
    automata: &BTreeMap<String, Automaton>,
    entity_name: &str,
    automaton: &Automaton,
    findings: &mut Vec<BundleLintFinding>,
) {
    for state in &automaton.state {
        if state.var_type == "ref"
            && let Some(target) = state.entity_type.as_deref()
            && !automata.contains_key(target)
        {
            findings.push(BundleLintFinding::error(
                entity_name,
                "reference_target_missing",
                format!(
                    "typed reference state variable '{}' targets unknown entity type '{}'",
                    state.name, target
                ),
            ));
        }
    }
    for action in &automaton.actions {
        for param in &action.params {
            if param.param_type() == "ref"
                && let Some(target) = param.entity_type()
                && !automata.contains_key(target)
            {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "reference_param_target_missing",
                    format!(
                        "typed reference parameter '{}.{}' targets unknown entity type '{}'",
                        action.name,
                        param.name(),
                        target
                    ),
                ));
            }
        }
    }
}

/// Cross-check any CSDL referential constraints that describe an ADR-0156
/// typed reference. CSDL navigation metadata is optional, but when present it
/// must agree exactly with the IOA target and the target entity key.
pub fn lint_csdl_reference_contracts(
    csdl: &crate::csdl::CsdlDocument,
    automata: &BTreeMap<String, Automaton>,
) -> Vec<BundleLintFinding> {
    let entity_types: BTreeMap<&str, &crate::csdl::EntityType> = csdl
        .schemas
        .iter()
        .flat_map(|schema| schema.entity_types.iter())
        .map(|entity| (entity.name.as_str(), entity))
        .collect();
    let mut findings = Vec::new();
    for (entity_name, automaton) in automata {
        let Some(csdl_entity) = entity_types.get(entity_name.as_str()) else {
            continue;
        };
        for state in automaton
            .state
            .iter()
            .filter(|state| state.var_type == "ref")
        {
            let expected_target = state.entity_type.as_deref().unwrap_or_default();
            for navigation in &csdl_entity.navigation_properties {
                for constraint in navigation
                    .referential_constraints
                    .iter()
                    .filter(|constraint| {
                        constraint.property == state.name
                            || constraint.property == crate::to_pascal_case(&state.name)
                            || crate::to_snake_case(&constraint.property)
                                == crate::to_snake_case(&state.name)
                    })
                {
                    let declared_target = navigation
                        .type_name
                        .trim_start_matches("Collection(")
                        .trim_end_matches(')')
                        .rsplit('.')
                        .next()
                        .unwrap_or(&navigation.type_name);
                    let target_key_matches =
                        entity_types.get(expected_target).is_some_and(|target| {
                            target
                                .key_properties
                                .iter()
                                .any(|key| key == &constraint.referenced_property)
                        });
                    if declared_target != expected_target || !target_key_matches {
                        findings.push(BundleLintFinding::error(
                            entity_name,
                            "csdl_reference_contract_mismatch",
                            format!(
                                "CSDL reference '{}' -> '{}.{}' contradicts typed reference '{}' -> '{}' target key",
                                constraint.property,
                                declared_target,
                                constraint.referenced_property,
                                state.name,
                                expected_target
                            ),
                        ));
                    }
                }
            }
        }
    }
    findings.sort_by(|left, right| {
        (&left.entity, &left.code, &left.message).cmp(&(&right.entity, &right.code, &right.message))
    });
    findings
}
