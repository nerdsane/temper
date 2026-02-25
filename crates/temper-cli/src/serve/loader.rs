//! Spec file loading, linting, and trajectory hydration.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use temper_runtime::tenant::TenantId;
use temper_server::event_store::ServerEventStore;
use temper_server::reaction::registry::parse_reactions;
use temper_server::registry::SpecRegistry;
use temper_spec::automaton::{LintSeverity, lint_automaton, parse_automaton};
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, lint_cross_invariants, parse_cross_invariants,
};
use temper_spec::csdl::{CsdlDocument, parse_csdl};

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
        if !entity_set_types.contains(entity) {
            findings.push(TenantLintFinding {
                entity: entity.clone(),
                code: "ioa_missing_entity_set".to_string(),
                severity: LintSeverity::Warning,
                message: "spec has no corresponding entity set in model.csdl.xml".to_string(),
            });
        }
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

    if !specs_path.is_dir() {
        anyhow::bail!("Specs directory not found: {}", specs_path.display());
    }

    // Read CSDL model
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        anyhow::bail!(
            "CSDL model not found at {}. Run `temper init` first.",
            csdl_path.display()
        );
    }

    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;

    // Read IOA TOML specs
    let ioa_sources = read_ioa_sources(specs_path)?;
    let reactions = read_reactions(specs_path)?;
    let cross_invariants_toml = read_cross_invariants_toml(specs_path)?;

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
            reactions,
            cross_invariants_toml.clone(),
        )
        .with_context(|| format!("Failed to register tenant '{tenant}'"))?;

    Ok(LoadedTenantSpecs {
        csdl_xml: registry
            .get_tenant(&TenantId::new(tenant))
            .map(|cfg| cfg.csdl_xml.as_ref().clone())
            .unwrap_or_default(),
        ioa_sources,
        cross_invariants_toml,
    })
}

/// Read all `.ioa.toml` files from the specs directory.
pub(super) fn read_ioa_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
    let mut sources = HashMap::new();

    for entry in fs::read_dir(specs_dir)
        .with_context(|| format!("Failed to read specs directory: {}", specs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name.ends_with(".ioa.toml") {
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);

            let source = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read IOA file: {}", path.display()))?;

            sources.insert(entity_name, source);
        }
    }

    Ok(sources)
}

/// Read optional `reactions.toml` and parse it into reaction rules.
pub(super) fn read_reactions(
    specs_dir: &Path,
) -> Result<Vec<temper_server::reaction::ReactionRule>> {
    let reactions_path = specs_dir.join("reactions.toml");
    if !reactions_path.exists() {
        return Ok(Vec::new());
    }

    let source = fs::read_to_string(&reactions_path)
        .with_context(|| format!("Failed to read {}", reactions_path.display()))?;
    parse_reactions(&source)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {e}", reactions_path.display()))
}

/// Read optional `cross-invariants.toml` source from a specs directory.
pub(super) fn read_cross_invariants_toml(specs_dir: &Path) -> Result<Option<String>> {
    let path = specs_dir.join("cross-invariants.toml");
    if !path.exists() {
        return Ok(None);
    }
    let source =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(Some(source))
}

/// Convert a string to PascalCase.
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

type PostgresTrajectoryRow = (
    String,
    String,
    String,
    String,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
);

/// Hydrate the in-memory trajectory log from the persistent backend.
pub(super) async fn hydrate_trajectory_log(
    server: &temper_server::state::ServerState,
    store: &std::sync::Arc<ServerEventStore>,
    apps: &[(String, String)],
) {
    use temper_server::state::TrajectoryEntry;

    // Try Postgres first (uses the trajectories table directly via sqlx).
    if let Some(pool) = store.postgres_pool() {
        let rows: Result<Vec<PostgresTrajectoryRow>, _> = sqlx::query_as(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, created_at \
             FROM trajectories \
             ORDER BY created_at DESC \
             LIMIT 10000",
        )
        .fetch_all(pool)
        .await;

        if let Ok(rows) = rows
            && let Ok(mut log) = server.trajectory_log.write()
        {
            // Insert oldest-first (rows are newest-first from query).
            for (
                tenant,
                entity_type,
                entity_id,
                action,
                success,
                from_status,
                to_status,
                error,
                created_at,
            ) in rows.into_iter().rev()
            {
                log.push(TrajectoryEntry {
                    timestamp: created_at.to_rfc3339(),
                    tenant,
                    entity_type,
                    entity_id,
                    action,
                    success,
                    from_status,
                    to_status,
                    error,
                    agent_id: None,
                    session_id: None,
                });
            }
            let count = log.entries().len();
            if count > 0 {
                println!("  Restored {count} trajectory entries from Postgres.");
            }
        }
        return;
    }

    // Try Turso.
    if let Some(turso) = store.turso_store() {
        match turso.load_recent_trajectories(10_000).await {
            Ok(rows) => {
                if let Ok(mut log) = server.trajectory_log.write() {
                    // Rows come newest-first, insert oldest-first.
                    for row in rows.into_iter().rev() {
                        log.push(TrajectoryEntry {
                            timestamp: row.created_at,
                            tenant: row.tenant,
                            entity_type: row.entity_type,
                            entity_id: row.entity_id,
                            action: row.action,
                            success: row.success,
                            from_status: row.from_status,
                            to_status: row.to_status,
                            error: row.error,
                            agent_id: None,
                            session_id: None,
                        });
                    }
                    let count = log.entries().len();
                    if count > 0 {
                        println!("  Restored {count} trajectory entries from Turso.");
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to load trajectories from Turso: {e}");
            }
        }
        return;
    }

    // Try Redis (per-tenant capped lists).
    if let Some(redis) = store.redis_store() {
        for (tenant, _dir) in apps {
            match redis.load_recent_trajectories(tenant, 10_000).await {
                Ok(entries) => {
                    if let Ok(mut log) = server.trajectory_log.write() {
                        for json_str in entries {
                            if let Ok(entry) = serde_json::from_str::<TrajectoryEntry>(&json_str) {
                                log.push(entry);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: failed to load trajectories from Redis for tenant {tenant}: {e}"
                    );
                }
            }
        }
        let count = server
            .trajectory_log
            .read()
            .map(|log| log.entries().len())
            .unwrap_or(0);
        if count > 0 {
            println!("  Restored {count} trajectory entries from Redis.");
        }
    }
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
}
