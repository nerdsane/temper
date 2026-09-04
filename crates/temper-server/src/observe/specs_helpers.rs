use std::collections::BTreeMap;

use axum::http::StatusCode;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{LintSeverity, lint_automata_bundle, lint_automaton};
use temper_spec::cross_invariant::{CrossInvariantLintFinding, CrossInvariantLintSeverity};

use crate::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};

pub(super) use temper_spec::naming::to_pascal_case;

/// Specs in this submission that are byte-identical to an already-passed
/// registry entry. The load-inline/load-dir cascade must not re-run for these:
/// that is the 60s+ CPU class when an agent re-submits an unchanged app.
pub(super) fn unchanged_passed_verification(
    registry: &SpecRegistry,
    tenant: &str,
    ioa_sources: &BTreeMap<String, String>,
) -> BTreeMap<String, EntityVerificationResult> {
    assert!(!tenant.is_empty(), "tenant is required for hash gating");
    let tenant_id = TenantId::new(tenant);
    let mut cached = BTreeMap::new();
    for (entity_type, source) in ioa_sources {
        let Some(existing) = registry.get_spec(&tenant_id, entity_type) else {
            continue;
        };
        if temper_store_turso::spec_content_hash(&existing.ioa_source)
            != temper_store_turso::spec_content_hash(source)
        {
            continue;
        }
        let Some(status) = registry.get_verification_status(&tenant_id, entity_type) else {
            continue;
        };
        match status {
            VerificationStatus::Completed(result) | VerificationStatus::Restored(result)
                if result.all_passed =>
            {
                cached.insert(entity_type.clone(), result.clone());
            }
            _ => {}
        }
    }
    cached
}

#[derive(Debug, Clone)]
pub(super) struct EntityLintFinding {
    pub(super) entity: String,
    pub(super) code: String,
    pub(super) severity: LintSeverity,
    pub(super) message: String,
}

pub(super) fn lint_loaded_specs(
    csdl: &temper_spec::csdl::CsdlDocument,
    ioa_sources: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<EntityLintFinding>, (StatusCode, String)> {
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

    for (entity_name, source) in ioa_sources {
        let automaton = temper_spec::automaton::parse_automaton(source).map_err(|e| {
            tracing::warn!(entity = %entity_name, error = %e, "IOA spec parse failure");
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse IOA spec for {entity_name}: {e}"),
            )
        })?;

        for finding in lint_automaton(&automaton) {
            findings.push(EntityLintFinding {
                entity: entity_name.clone(),
                code: finding.code,
                severity: finding.severity,
                message: finding.message,
            });
        }
        parsed_automata.insert(entity_name.clone(), automaton);

        if !entity_set_types.contains(entity_name) {
            findings.push(EntityLintFinding {
                entity: entity_name.clone(),
                code: "ioa_missing_entity_set".to_string(),
                severity: LintSeverity::Warning,
                message: "spec has no corresponding entity set in model.csdl.xml".to_string(),
            });
        }
    }

    for finding in lint_automata_bundle(&parsed_automata) {
        findings.push(EntityLintFinding {
            entity: finding.entity,
            code: finding.code,
            severity: finding.severity,
            message: finding.message,
        });
    }

    for entity_type in &entity_set_types {
        if !ioa_sources.contains_key(entity_type) {
            findings.push(EntityLintFinding {
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

pub(super) fn lint_ndjson_line(finding: &EntityLintFinding) -> serde_json::Value {
    serde_json::json!({
        "type": match finding.severity {
            LintSeverity::Error => "lint_error",
            LintSeverity::Warning => "lint_warning",
        },
        "severity": match finding.severity {
            LintSeverity::Error => "error",
            LintSeverity::Warning => "warning",
        },
        "entity": &finding.entity,
        "code": &finding.code,
        "message": &finding.message,
    })
}

pub(super) fn cross_lint_ndjson_line(finding: &CrossInvariantLintFinding) -> serde_json::Value {
    serde_json::json!({
        "type": match finding.severity {
            CrossInvariantLintSeverity::Error => "cross_invariant_lint_error",
            CrossInvariantLintSeverity::Warning => "cross_invariant_lint_warning",
        },
        "severity": match finding.severity {
            CrossInvariantLintSeverity::Error => "error",
            CrossInvariantLintSeverity::Warning => "warning",
        },
        "invariant": &finding.invariant,
        "code": &finding.code,
        "message": &finding.message,
    })
}

pub(super) fn build_ndjson_response(
    status: StatusCode,
    lines: Vec<serde_json::Value>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let mut body = String::new();
    for line in lines {
        let encoded = serde_json::to_string(&line).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to encode NDJSON response: {e}"),
            )
        })?;
        body.push_str(&encoded);
        body.push('\n');
    }

    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/x-ndjson")
        .body(axum::body::Body::from(body))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build NDJSON response: {e}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
    use temper_spec::csdl::parse_csdl;

    const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn passed_result() -> EntityVerificationResult {
        EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0_symbolic".to_string(),
                passed: true,
                summary: "cached".to_string(),
                details: None,
            }],
            verified_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn unchanged_passed_verification_skips_only_identical_passed_specs() {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant("t", csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
        let tenant = TenantId::new("t");
        registry.set_verification_status(
            &tenant,
            "Order",
            VerificationStatus::Completed(passed_result()),
        );

        let same = BTreeMap::from([("Order".to_string(), ORDER_IOA.to_string())]);
        let cached = unchanged_passed_verification(&registry, "t", &same);
        assert_eq!(cached.len(), 1);
        assert!(cached["Order"].all_passed);

        let changed = BTreeMap::from([("Order".to_string(), format!("{ORDER_IOA}\n# touch\n"))]);
        assert!(unchanged_passed_verification(&registry, "t", &changed).is_empty());
    }

    #[test]
    fn unchanged_passed_verification_ignores_pending_and_failed() {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant("t", csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
        let tenant = TenantId::new("t");
        registry.set_verification_status(&tenant, "Order", VerificationStatus::Pending);
        let same = BTreeMap::from([("Order".to_string(), ORDER_IOA.to_string())]);
        assert!(unchanged_passed_verification(&registry, "t", &same).is_empty());

        let mut failed = passed_result();
        failed.all_passed = false;
        registry.set_verification_status(&tenant, "Order", VerificationStatus::Completed(failed));
        assert!(unchanged_passed_verification(&registry, "t", &same).is_empty());
    }
}
