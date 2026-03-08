//! Cross-entity relation and invariant enforcement.

use std::time::Instant; // determinism-ok: scoped duration measurement only

use tracing::instrument;

use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant::{
    CrossInvariant, DeletePolicy, InvariantKind, parse_related_status_in_assert,
};

use crate::registry::RelationEdge;
use crate::state::ServerState;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintViolationType {
    RelationIntegrity,
    CrossInvariant,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConstraintViolation {
    pub violation_type: ConstraintViolationType,
    pub invariant: Option<String>,
    pub message: String,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
}

impl ConstraintViolation {
    fn relation(
        message: impl Into<String>,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
    ) -> Self {
        Self {
            violation_type: ConstraintViolationType::RelationIntegrity,
            invariant: None,
            message: message.into(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            operation: operation.to_string(),
        }
    }

    fn invariant(
        invariant: &str,
        message: impl Into<String>,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
    ) -> Self {
        Self {
            violation_type: ConstraintViolationType::CrossInvariant,
            invariant: Some(invariant.to_string()),
            message: message.into(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            operation: operation.to_string(),
        }
    }
}

/// Check FK integrity for create/update style writes.
#[instrument(skip_all, fields(otel.name = "constraint.pre_upsert_relation_checks", tenant = %tenant, entity_type, entity_id, operation))]
pub async fn pre_upsert_relation_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    fields: &serde_json::Value,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let (tenant_name, edges): (String, Vec<RelationEdge>) = {
        let registry = state.registry.read().unwrap();
        let Some(tc) = registry.get_tenant(tenant) else {
            return Ok(());
        };
        (
            tenant.to_string(),
            tc.relation_graph
                .outgoing
                .get(entity_type)
                .cloned()
                .unwrap_or_default(),
        )
    };

    for edge in edges {
        let Some(value) = extract_field(fields, &edge.source_field) else {
            continue;
        };
        if value.is_null() {
            if !edge.nullable {
                tracing::warn!(
                    tenant = %tenant_name, entity_type, entity_id, operation,
                    field = %edge.source_field,
                    "constraint violation: non-nullable relation field is null"
                );
                state.metrics.record_relation_integrity_violation(
                    &tenant_name,
                    entity_type,
                    operation,
                );
                return Err(ConstraintViolation::relation(
                    format!(
                        "non-nullable relation field '{}' cannot be null",
                        edge.source_field
                    ),
                    entity_type,
                    entity_id,
                    operation,
                ));
            }
            continue;
        }
        let Some(target_id) = value.as_str() else {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, operation,
                field = %edge.source_field,
                "constraint violation: relation field is not a string ID"
            );
            state
                .metrics
                .record_relation_integrity_violation(&tenant_name, entity_type, operation);
            return Err(ConstraintViolation::relation(
                format!("relation field '{}' must be a string ID", edge.source_field),
                entity_type,
                entity_id,
                operation,
            ));
        };
        if !state
            .ensure_entity_loaded(tenant, &edge.to_entity, target_id)
            .await
        {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, operation,
                target_entity = %edge.to_entity, target_id,
                "constraint violation: relation target not found"
            );
            state
                .metrics
                .record_relation_integrity_violation(&tenant_name, entity_type, operation);
            return Err(ConstraintViolation::relation(
                format!(
                    "relation target '{}' with id '{}' not found (from {}.{})",
                    edge.to_entity, target_id, entity_type, edge.source_field
                ),
                entity_type,
                entity_id,
                operation,
            ));
        }
    }

    Ok(())
}

/// Check incoming relation policy before deleting an entity.
#[instrument(skip_all, fields(otel.name = "constraint.pre_delete_relation_checks", tenant = %tenant, entity_type, entity_id, operation))]
pub async fn pre_delete_relation_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let (tenant_name, edges): (String, Vec<RelationEdge>) = {
        let registry = state.registry.read().unwrap();
        let Some(tc) = registry.get_tenant(tenant) else {
            return Ok(());
        };
        (
            tenant.to_string(),
            tc.relation_graph
                .incoming
                .get(entity_type)
                .cloned()
                .unwrap_or_default(),
        )
    };

    for edge in edges {
        if edge.delete_policy != DeletePolicy::Restrict {
            continue;
        }
        let source_ids = state.list_entity_ids_lazy(tenant, &edge.from_entity).await;
        for source_id in source_ids {
            if let Ok(source_state) = state
                .get_tenant_entity_state(tenant, &edge.from_entity, &source_id)
                .await
            {
                let source_fields =
                    serde_json::to_value(&source_state.state.fields).unwrap_or_default();
                if extract_field_as_str(&source_fields, &edge.source_field) == Some(entity_id) {
                    tracing::warn!(
                        tenant = %tenant_name, entity_type, entity_id, operation,
                        from_entity = %edge.from_entity, source_id = %source_id,
                        "constraint violation: cannot delete entity referenced by another"
                    );
                    state.metrics.record_relation_integrity_violation(
                        &tenant_name,
                        entity_type,
                        operation,
                    );
                    return Err(ConstraintViolation::relation(
                        format!(
                            "cannot delete {}('{}'): referenced by {}('{}') via {}",
                            entity_type, entity_id, edge.from_entity, source_id, edge.source_field
                        ),
                        entity_type,
                        entity_id,
                        operation,
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Evaluate cross-entity invariants triggered by a write.
#[instrument(skip_all, fields(otel.name = "constraint.post_write_invariant_checks", tenant = %tenant, entity_type, entity_id, action_name = action, operation))]
pub async fn post_write_invariant_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    fields: &serde_json::Value,
    operation: &str,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let start = Instant::now(); // determinism-ok: scoped duration measurement, not simulation-visible state
    let (tenant_name, invariants): (String, Vec<CrossInvariant>) = {
        let registry = state.registry.read().unwrap();
        let Some(tc) = registry.get_tenant(tenant) else {
            return Ok(());
        };
        (
            tenant.to_string(),
            tc.cross_invariants
                .as_ref()
                .map(|c| c.invariants.clone())
                .unwrap_or_default(),
        )
    };

    for inv in invariants {
        if !trigger_matches(&inv.on, entity_type, action) {
            continue;
        }
        state
            .metrics
            .record_cross_invariant_check(&tenant_name, entity_type, "evaluated");
        let Some(assertion) = parse_related_status_in_assert(&inv.assertion) else {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                "constraint violation: invalid assertion syntax"
            );
            state.metrics.record_cross_invariant_violation(
                &tenant_name,
                &inv.name,
                "invalid_assertion",
            );
            return Err(ConstraintViolation::invariant(
                &inv.name,
                "invalid assertion syntax",
                entity_type,
                entity_id,
                operation,
            ));
        };

        let Some(target_id) = extract_field_as_str(fields, &assertion.source_field) else {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                source_field = %assertion.source_field,
                "constraint violation: source field required by invariant is missing"
            );
            state.metrics.record_cross_invariant_violation(
                &tenant_name,
                &inv.name,
                "missing_source_field",
            );
            return Err(ConstraintViolation::invariant(
                &inv.name,
                format!(
                    "source field '{}' required by invariant is missing",
                    assertion.source_field
                ),
                entity_type,
                entity_id,
                operation,
            ));
        };

        if !state
            .ensure_entity_loaded(tenant, &assertion.target_entity, target_id)
            .await
        {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                target_entity = %assertion.target_entity, target_id,
                "constraint violation: related entity not found"
            );
            state.metrics.record_cross_invariant_violation(
                &tenant_name,
                &inv.name,
                "target_missing",
            );
            let violation = ConstraintViolation::invariant(
                &inv.name,
                format!(
                    "related entity {}('{}') not found",
                    assertion.target_entity, target_id
                ),
                entity_type,
                entity_id,
                operation,
            );
            if inv.kind == InvariantKind::Eventual {
                defer_eventual_invariant(state, &inv, &tenant_name, entity_type, entity_id);
                continue;
            }
            return Err(violation);
        }

        let target_status = match state
            .get_tenant_entity_state(tenant, &assertion.target_entity, target_id)
            .await
        {
            Ok(resp) => resp.state.status,
            Err(e) => {
                state.metrics.record_cross_invariant_violation(
                    &tenant_name,
                    &inv.name,
                    "target_read_error",
                );
                let violation = ConstraintViolation::invariant(
                    &inv.name,
                    format!("failed to read related entity state: {e}"),
                    entity_type,
                    entity_id,
                    operation,
                );
                if inv.kind == InvariantKind::Eventual {
                    defer_eventual_invariant(state, &inv, &tenant_name, entity_type, entity_id);
                    continue;
                }
                return Err(violation);
            }
        };

        if !assertion.statuses.iter().any(|s| s == &target_status) {
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                target_entity = %assertion.target_entity, target_id, target_status = %target_status,
                expected = ?assertion.statuses,
                "constraint violation: related entity status mismatch"
            );
            state.metrics.record_cross_invariant_violation(
                &tenant_name,
                &inv.name,
                "status_mismatch",
            );
            let violation = ConstraintViolation::invariant(
                &inv.name,
                format!(
                    "related {}('{}') has status '{}', expected one of {:?}",
                    assertion.target_entity, target_id, target_status, assertion.statuses
                ),
                entity_type,
                entity_id,
                operation,
            );
            if inv.kind == InvariantKind::Eventual {
                defer_eventual_invariant(state, &inv, &tenant_name, entity_type, entity_id);
                continue;
            }
            return Err(violation);
        }
    }

    state
        .metrics
        .record_cross_eval_duration_ms(start.elapsed().as_millis() as u64);
    Ok(())
}

/// Defer an eventual invariant to the background convergence tracker.
fn defer_eventual_invariant(
    state: &ServerState,
    inv: &CrossInvariant,
    tenant_name: &str,
    entity_type: &str,
    entity_id: &str,
) {
    let window = inv.window_ms.unwrap_or(5000);
    let tracker_ok = state
        .eventual_tracker
        .write()
        .unwrap() // ci-ok: infallible lock
        .record(&inv.name, tenant_name, entity_type, entity_id, window);
    if !tracker_ok {
        tracing::warn!(
            invariant = %inv.name,
            "eventual invariant tracker budget exhausted"
        );
    }
    state
        .metrics
        .record_cross_invariant_check(tenant_name, entity_type, "eventual_deferred");
}

fn trigger_matches(on: &str, entity_type: &str, action: &str) -> bool {
    let Some((entity, action_sel)) = on.split_once('.') else {
        return false;
    };
    if entity.trim() != entity_type {
        return false;
    }
    let action_sel = action_sel.trim();
    action_sel == "*" || action_sel == action
}

fn extract_field<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    fields.get(name).or_else(|| {
        fields
            .get("fields")
            .and_then(|f| f.as_object())
            .and_then(|obj| obj.get(name))
    })
}

fn extract_field_as_str<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    extract_field(fields, name).and_then(|v| v.as_str())
}
