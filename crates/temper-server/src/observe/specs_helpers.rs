use axum::http::StatusCode;
use temper_spec::automaton::{LintSeverity, lint_automaton};
use temper_spec::cross_invariant::{CrossInvariantLintFinding, CrossInvariantLintSeverity};

#[derive(Debug, Clone)]
pub(super) struct EntityLintFinding {
    pub(super) entity: String,
    pub(super) code: String,
    pub(super) severity: LintSeverity,
    pub(super) message: String,
}

/// Convert a string to PascalCase (e.g. "my_entity" -> "MyEntity").
pub(super) fn to_pascal_case(s: &str) -> String {
    s.split(['_', '-'])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}

pub(super) fn lint_loaded_specs(
    csdl: &temper_spec::csdl::CsdlDocument,
    ioa_sources: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<EntityLintFinding>, (StatusCode, String)> {
    let mut findings = Vec::new();
    let mut entity_set_types = std::collections::BTreeSet::new();

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

        if !entity_set_types.contains(entity_name) {
            findings.push(EntityLintFinding {
                entity: entity_name.clone(),
                code: "ioa_missing_entity_set".to_string(),
                severity: LintSeverity::Warning,
                message: "spec has no corresponding entity set in model.csdl.xml".to_string(),
            });
        }
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
