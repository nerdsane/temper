//! Spec file loading, linting, and trajectory hydration.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};

use crate::util::to_pascal_case;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_spec::automaton::{LintSeverity, lint_automata_bundle, lint_automaton, parse_automaton};
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, lint_cross_invariants, parse_cross_invariants,
};
use temper_spec::csdl::{CsdlDocument, parse_csdl};
use temper_spec::loader::CedarPolicySource;

use super::LoadedTenantSpecs;

#[derive(Debug, Clone)]
pub(super) struct TenantLintFinding {
    pub entity: String,
    pub code: String,
    pub severity: LintSeverity,
    pub message: String,
}

pub(super) fn lint_tenant_specs(
    csdl: &CsdlDocument,
    ioa_sources: &HashMap<String, String>,
) -> Result<Vec<TenantLintFinding>> {
    let mut findings = Vec::new();
    let mut entity_set_types = std::collections::BTreeSet::new();
    let mut parsed_automata = std::collections::BTreeMap::new();

    for schema in &csdl.schemas {
        for container in &schema.entity_containers {
            for entity_set in &container.entity_sets {
                let type_name = entity_set
                    .entity_type
                    .rsplit('.')
                    .next()
                    .unwrap_or(&entity_set.entity_type);
                entity_set_types.insert(type_name.to_string());
            }
        }
    }

    for (entity, source) in ioa_sources {
        let automaton = parse_automaton(source)
            .with_context(|| format!("failed to parse IOA spec for {entity}"))?;
        for finding in lint_automaton(&automaton) {
            findings.push(TenantLintFinding {
                entity: entity.clone(),
                code: finding.code,
                severity: finding.severity,
                message: finding.message,
            });
        }
        parsed_automata.insert(entity.clone(), automaton);
        if !entity_set_types.contains(entity) {
            findings.push(TenantLintFinding {
                entity: entity.clone(),
                code: "ioa_missing_entity_set".to_string(),
                severity: LintSeverity::Warning,
                message: "spec has no corresponding entity set in model.csdl.xml".to_string(),
            });
        }
    }

    for finding in lint_automata_bundle(&parsed_automata) {
        findings.push(TenantLintFinding {
            entity: finding.entity,
            code: finding.code,
            severity: finding.severity,
            message: finding.message,
        });
    }

    for entity_type in &entity_set_types {
        if !ioa_sources.contains_key(entity_type) {
            findings.push(TenantLintFinding {
                entity: entity_type.clone(),
                code: "csdl_missing_ioa_spec".to_string(),
                severity: LintSeverity::Warning,
                message: "entity set has no corresponding IOA spec".to_string(),
            });
        }
    }

    findings.sort_by(|a, b| {
        let key_a = (
            &a.entity,
            matches!(a.severity, LintSeverity::Warning),
            &a.code,
            &a.message,
        );
        let key_b = (
            &b.entity,
            matches!(b.severity, LintSeverity::Warning),
            &b.code,
            &b.message,
        );
        key_a.cmp(&key_b)
    });

    Ok(findings)
}

/// Load specs from a directory into an existing SpecRegistry WITHOUT running verification.
///
/// All entities start with `VerificationStatus::Pending`. The observe UI
/// can display state machines immediately while verification runs in background.
pub(super) fn load_into_registry(
    registry: &mut SpecRegistry,
    specs_dir: &str,
    tenant: &str,
) -> Result<LoadedTenantSpecs> {
    let specs_path = Path::new(specs_dir);

    // ADR-0046 hard cut: standalone reaction files are no longer supported
    // (same migration error as the platform OS-app loader).
    let legacy_reactions_path = specs_path.join("reactions.toml");
    if legacy_reactions_path.exists() {
        anyhow::bail!(
            "legacy reactions.toml is no longer supported; migrate this app to inline \
             [[action.triggers]] (found {})",
            legacy_reactions_path.display()
        );
    }

    let bundle = temper_spec::loader::load_spec_dir(specs_path).map_err(|e| anyhow::anyhow!(e))?;

    // CSDL model is mandatory for `temper serve` tenants.
    let Some(csdl_source) = bundle.csdl else {
        anyhow::bail!(
            "CSDL model not found at {} (also checked specs/ and csdl/). Run `temper init` first.",
            specs_path.join("model.csdl.xml").display()
        );
    };
    let csdl_xml = csdl_source.content;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_source.path.display()))?;

    // Entity types come from each automaton's `name` field, so two files can
    // collide. Fail loudly instead of silently dropping one spec.
    let mut ioa_sources: HashMap<String, String> = HashMap::new();
    for (entity_type, source) in bundle.specs {
        if ioa_sources.insert(entity_type.clone(), source).is_some() {
            anyhow::bail!(
                "duplicate entity type '{entity_type}' in {}: two .ioa.toml files declare the \
                 same [automaton] name",
                specs_path.display()
            );
        }
    }
    let cross_invariants_toml = bundle.cross_invariants_toml;
    let cedar_policy_text = build_tenant_cedar_policy(&bundle.cedar_policies, ioa_sources.keys())?;

    let lint_findings = lint_tenant_specs(&csdl, &ioa_sources)?;
    let mut lint_errors = Vec::new();
    for finding in &lint_findings {
        match finding.severity {
            LintSeverity::Error => lint_errors.push(format!(
                "    [lint:error:{}] {}: {}",
                finding.code, finding.entity, finding.message
            )),
            LintSeverity::Warning => eprintln!(
                "    [lint:warning:{}] {}: {}",
                finding.code, finding.entity, finding.message
            ),
        }
    }

    if let Some(source) = cross_invariants_toml.as_deref() {
        let parsed = parse_cross_invariants(source).with_context(|| {
            format!(
                "Failed to parse cross-invariants.toml for tenant '{}'",
                tenant
            )
        })?;
        let xinv_findings = lint_cross_invariants(&parsed);
        for finding in xinv_findings {
            match finding.severity {
                CrossInvariantLintSeverity::Error => lint_errors.push(format!(
                    "    [xinv:error:{}] {}",
                    finding.code, finding.message
                )),
                CrossInvariantLintSeverity::Warning => {
                    eprintln!("    [xinv:warning:{}] {}", finding.code, finding.message)
                }
            }
        }
    }

    if !lint_errors.is_empty() {
        anyhow::bail!(
            "Semantic lint failed for tenant '{}':\n{}",
            tenant,
            lint_errors.join("\n")
        );
    }

    for entity_name in ioa_sources.keys() {
        println!("    Loaded spec: {entity_name} (verification pending, lint clean)");
    }

    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    registry
        .try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            &ioa_pairs,
            Vec::new(),
            cross_invariants_toml.clone(),
            false,
        )
        .with_context(|| format!("Failed to register tenant '{tenant}'"))?;

    Ok(LoadedTenantSpecs {
        csdl_xml: registry
            .get_tenant(&TenantId::new(tenant))
            .map(|cfg| cfg.csdl_xml.as_ref().clone())
            .unwrap_or_default(),
        ioa_sources,
        cross_invariants_toml,
        cedar_policy_text,
    })
}

/// Build tenant Cedar policy text from discovered `policies/*.cedar` files.
///
/// Behavior:
/// - Concatenates all `.cedar` files in lexical filename order (the order
///   `temper_spec::loader` discovered them in).
/// - Tracks entity-scoped files by stem (e.g. `order.cedar` -> `Order`).
/// - For each entity type without a corresponding `.cedar` file, appends a
///   generated permit-all fallback policy so legacy entities stay operable.
/// - Leaves all other resource types at Cedar default-deny.
pub(super) fn build_tenant_cedar_policy<'a>(
    cedar_policies: &[CedarPolicySource],
    entity_types: impl Iterator<Item = &'a String>,
) -> Result<Option<String>> {
    let mut policy_chunks: Vec<String> = Vec::new();
    let mut covered_entities = BTreeSet::new();

    for policy in cedar_policies {
        if !policy.content.trim().is_empty() {
            policy_chunks.push(policy.content.clone());
        }
        covered_entities.insert(to_pascal_case(&policy.stem));
    }

    let mut entities: Vec<String> = entity_types.cloned().collect();
    entities.sort();
    for entity in entities {
        if covered_entities.contains(&entity) {
            continue;
        }
        policy_chunks.push(format!(
            "permit(\n    principal,\n    action,\n    resource is {entity}\n);"
        ));
    }

    if policy_chunks.is_empty() {
        return Ok(None);
    }

    let combined = policy_chunks.join("\n\n");
    temper_authz::AuthzEngine::new(&combined)
        .map_err(|e| anyhow::anyhow!("Failed to parse combined Cedar policies: {e}"))?;
    Ok(Some(combined))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

    #[test]
    fn lint_tenant_specs_flags_unknown_variables() {
        let csdl = parse_csdl(TEST_CSDL).expect("CSDL should parse");
        let mut ioa_sources = HashMap::new();
        ioa_sources.insert(
            "Order".to_string(),
            r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = "set phantom true"
"#
            .to_string(),
        );

        let findings = lint_tenant_specs(&csdl, &ioa_sources).expect("lint should run");
        assert!(
            findings
                .iter()
                .any(|f| f.code == "effect_unknown_var" && f.severity == LintSeverity::Error)
        );
    }

    #[test]
    fn load_into_registry_rejects_lint_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("model.csdl.xml"), TEST_CSDL).expect("write csdl"); // determinism-ok: test-only
        std::fs::write(
            // determinism-ok: test-only
            tmp.path().join("order.ioa.toml"),
            r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = "set phantom true"
"#,
        )
        .expect("write ioa");

        let mut registry = SpecRegistry::new();
        let err = match load_into_registry(
            &mut registry,
            tmp.path().to_str().expect("utf8 path"),
            "lint-tenant",
        ) {
            Ok(_) => panic!("lint errors should abort loading"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Semantic lint failed"));
        assert!(registry.get_tenant(&TenantId::new("lint-tenant")).is_none());
    }

    #[test]
    fn load_into_registry_rejects_legacy_reactions_toml() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("model.csdl.xml"), TEST_CSDL).expect("write csdl"); // determinism-ok: test-only
        std::fs::write(tmp.path().join("reactions.toml"), "[[reaction]]\n").expect("write legacy"); // determinism-ok: test-only

        let mut registry = SpecRegistry::new();
        let err = match load_into_registry(
            &mut registry,
            tmp.path().to_str().expect("utf8 path"),
            "legacy-reactions-tenant",
        ) {
            Ok(_) => panic!("legacy reactions.toml should abort loading"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("legacy reactions.toml is no longer supported"),
            "got: {err}"
        );
    }

    #[test]
    fn load_into_registry_uses_automaton_name_not_filename() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("model.csdl.xml"), TEST_CSDL).expect("write csdl"); // determinism-ok: test-only
        // File name would PascalCase to "Mismatched"; the automaton declares "Order".
        std::fs::write(
            // determinism-ok: test-only
            tmp.path().join("mismatched.ioa.toml"),
            r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
"#,
        )
        .expect("write ioa");

        let mut registry = SpecRegistry::new();
        let loaded = load_into_registry(
            &mut registry,
            tmp.path().to_str().expect("utf8 path"),
            "naming-tenant",
        )
        .expect("load should succeed");
        assert!(loaded.ioa_sources.contains_key("Order"));
        assert!(!loaded.ioa_sources.contains_key("Mismatched"));
    }

    #[test]
    fn build_tenant_cedar_policy_adds_permit_fallback_for_missing_entity_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let specs_dir = tmp.path();
        std::fs::create_dir_all(specs_dir.join("policies")).expect("policies dir");
        std::fs::write(
            specs_dir.join("policies").join("order.cedar"),
            "permit(principal, action, resource is Order);",
        )
        .expect("write order policy");

        let entities = ["Order".to_string(), "Issue".to_string()];
        let cedar_policies =
            temper_spec::loader::load_cedar_policies(specs_dir).expect("load cedar policies");
        let combined = build_tenant_cedar_policy(&cedar_policies, entities.iter())
            .expect("build policy")
            .expect("non-empty policy text");

        assert!(combined.contains("resource is Order"));
        assert!(combined.contains("resource is Issue"));

        let engine = temper_authz::AuthzEngine::new(&combined).expect("policy parses");
        let customer_ctx = temper_authz::SecurityContext::from_headers(&[
            ("x-temper-principal-id".to_string(), "cust-1".to_string()),
            (
                "x-temper-principal-kind".to_string(),
                "customer".to_string(),
            ),
        ]);

        let issue = engine.authorize(
            &customer_ctx,
            "read",
            "Issue",
            &std::collections::HashMap::from([(
                "id".to_string(),
                serde_json::Value::String("issue-1".to_string()),
            )]),
        );
        assert!(matches!(issue, temper_authz::AuthzDecision::Allow { .. }));

        let ungoverned = engine.authorize(
            &customer_ctx,
            "read",
            "UnguardedType",
            &std::collections::HashMap::from([(
                "id".to_string(),
                serde_json::Value::String("x".to_string()),
            )]),
        );
        assert!(matches!(
            ungoverned,
            temper_authz::AuthzDecision::Deny(temper_authz::AuthzDenial::NoMatchingPermit)
        ));
    }
}
