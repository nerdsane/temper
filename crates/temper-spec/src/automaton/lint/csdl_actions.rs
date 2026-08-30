//! Cross-format validation for callable IOA and bound CSDL actions.

use std::collections::{BTreeMap, BTreeSet};

use crate::automaton::Automaton;
use crate::csdl::CsdlDocument;

use super::BundleLintFinding;
use super::sort::sort_bundle_findings;

/// Validate the callable action contract shared by an IOA bundle and CSDL.
///
/// CSDL actions unrelated to an IOA action are left untouched. When an IOA
/// action and a bound CSDL action match by entity and action name, their
/// binding declaration and normalized non-binding parameter contracts must
/// agree exactly.
pub fn lint_automata_csdl_bundle(
    automata: &BTreeMap<String, Automaton>,
    csdl: &CsdlDocument,
) -> Vec<BundleLintFinding> {
    let mut findings = Vec::new();

    for (entity_name, automaton) in automata {
        for ioa_action in automaton
            .actions
            .iter()
            .filter(|action| action.kind != "output")
        {
            let candidates: Vec<_> = csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.actions)
                .filter(|candidate| candidate.name == ioa_action.name && candidate.is_bound)
                .collect();
            let matching: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|candidate| {
                    candidate
                        .parameters
                        .first()
                        .is_some_and(|binding| type_tail(&binding.type_name) == entity_name)
                })
                .collect();
            if matching.is_empty() && csdl_has_entity_type(csdl, entity_name) {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "csdl_action_missing",
                    format!(
                        "callable IOA action '{}.{}' has no matching bound CSDL action",
                        entity_name, ioa_action.name
                    ),
                ));
                continue;
            }
            for csdl_action in matching.iter().copied() {
                lint_csdl_action_contract(entity_name, ioa_action, csdl_action, &mut findings);
            }
        }
    }

    sort_bundle_findings(&mut findings);
    findings
}

fn lint_csdl_action_contract(
    entity_name: &str,
    ioa_action: &crate::automaton::Action,
    csdl_action: &crate::csdl::Action,
    findings: &mut Vec<BundleLintFinding>,
) {
    let Some(binding) = csdl_action.parameters.first() else {
        findings.push(BundleLintFinding::error(
            entity_name,
            "csdl_action_binding_nullable",
            format!(
                "bound CSDL action '{}.{}' has no binding parameter",
                entity_name, ioa_action.name
            ),
        ));
        return;
    };
    if binding.nullable {
        findings.push(BundleLintFinding::error(
            entity_name,
            "csdl_action_binding_nullable",
            format!(
                "bound CSDL action '{}.{}' binding parameter '{}' must be non-nullable",
                entity_name, ioa_action.name, binding.name
            ),
        ));
    }

    let mut ioa_params = BTreeMap::new();
    for param in &ioa_action.params {
        insert_normalized_param(
            entity_name,
            &ioa_action.name,
            "IOA",
            param.name(),
            param.nullable(),
            &mut ioa_params,
            findings,
        );
    }

    let mut csdl_params = BTreeMap::new();
    for param in csdl_action.parameters.iter().skip(1) {
        insert_normalized_param(
            entity_name,
            &ioa_action.name,
            "CSDL",
            &param.name,
            param.nullable,
            &mut csdl_params,
            findings,
        );
    }

    let names: BTreeSet<String> = ioa_params
        .keys()
        .chain(csdl_params.keys())
        .cloned()
        .collect();
    for normalized_name in names {
        match (
            ioa_params.get(&normalized_name),
            csdl_params.get(&normalized_name),
        ) {
            (Some(ioa), Some(csdl)) if ioa.1 != csdl.1 => {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "csdl_action_parameter_requiredness_mismatch",
                    format!(
                        "action '{}.{}' parameter '{}' nullability differs: IOA nullable={}, CSDL nullable={}",
                        entity_name, ioa_action.name, ioa.0, ioa.1, csdl.1
                    ),
                ));
            }
            (Some(_), Some(_)) => {}
            (Some(ioa), None) => findings.push(BundleLintFinding::error(
                entity_name,
                "csdl_action_parameter_mismatch",
                format!(
                    "action '{}.{}' parameter '{}' is declared by IOA but missing from CSDL",
                    entity_name, ioa_action.name, ioa.0
                ),
            )),
            (None, Some(csdl)) => findings.push(BundleLintFinding::error(
                entity_name,
                "csdl_action_parameter_mismatch",
                format!(
                    "action '{}.{}' parameter '{}' is declared by CSDL but missing from IOA",
                    entity_name, ioa_action.name, csdl.0
                ),
            )),
            (None, None) => unreachable!("parameter name came from one contract"),
        }
    }
}

fn insert_normalized_param(
    entity_name: &str,
    action_name: &str,
    source: &str,
    name: &str,
    nullable: bool,
    params: &mut BTreeMap<String, (String, bool)>,
    findings: &mut Vec<BundleLintFinding>,
) {
    let normalized = crate::naming::to_snake_case(name);
    if let Some(existing) = params.get(&normalized) {
        findings.push(BundleLintFinding::error(
            entity_name,
            "csdl_action_parameter_alias_collision",
            format!(
                "action '{}.{}' {source} parameters '{}' and '{}' normalize to '{}'",
                entity_name, action_name, existing.0, name, normalized
            ),
        ));
        return;
    }
    params.insert(normalized, (name.to_string(), nullable));
}

fn csdl_has_entity_type(csdl: &CsdlDocument, entity_name: &str) -> bool {
    csdl.schemas.iter().any(|schema| {
        schema
            .entity_types
            .iter()
            .any(|entity| entity.name == entity_name)
    })
}

fn type_tail(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap_or(type_name)
}
