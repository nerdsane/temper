//! Cross-entity relation and invariant enforcement.

use std::time::Instant; // determinism-ok: scoped duration measurement only

use tracing::instrument;

use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;
use temper_spec::cross_invariant::{
    CrossInvariant, CrossInvariantOperator, DeletePolicy, InvariantKind, parse_related_field_assert,
};

use crate::registry::RelationEdge;
use crate::state::ServerState;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintViolationType {
    RelationIntegrity,
    CrossInvariant,
    FieldInvariant,
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

    fn field_invariant(
        invariant: &str,
        message: impl Into<String>,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
    ) -> Self {
        Self {
            violation_type: ConstraintViolationType::FieldInvariant,
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
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let (tenant_name, edges): (String, Vec<RelationEdge>) = {
        let registry = state.registry.read().unwrap();
        let config = match schema_pin {
            Some(pin) => {
                registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            }
            None => registry.get_tenant(tenant),
        };
        let Some(tc) = config else {
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
        let target_exists = match schema_pin {
            Some(pin) => {
                state
                    .scoped_reference_target_exists(tenant, &edge.to_entity, target_id, pin)
                    .await
            }
            None => {
                state
                    .ensure_entity_loaded(tenant, &edge.to_entity, target_id)
                    .await
            }
        };
        if !target_exists {
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
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let (tenant_name, edges): (String, Vec<RelationEdge>) = {
        let registry = state.registry.read().unwrap();
        let config = match schema_pin {
            Some(pin) => {
                registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            }
            None => registry.get_tenant(tenant),
        };
        let Some(tc) = config else {
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
        const DELETE_RELATION_SCAN_BUDGET: usize = 10_000;
        let source_ids = match schema_pin {
            Some(pin) => {
                let ids = state
                    .list_scoped_entity_ids_bounded(
                        tenant,
                        &edge.from_entity,
                        pin,
                        DELETE_RELATION_SCAN_BUDGET + 1,
                    )
                    .await
                    .map_err(|error| {
                        ConstraintViolation::relation(error, entity_type, entity_id, operation)
                    })?;
                if ids.len() > DELETE_RELATION_SCAN_BUDGET {
                    return Err(ConstraintViolation::relation(
                        "scoped delete relation scan budget exhausted",
                        entity_type,
                        entity_id,
                        operation,
                    ));
                }
                ids
            }
            None => {
                crate::application_data::GovernedApplicationDataService::new(state)
                    .fallback_candidates(tenant, &edge.from_entity)
                    .await
            }
        };
        for source_id in source_ids {
            let source_state = match schema_pin {
                Some(pin) => {
                    state
                        .get_scoped_entity_state(tenant, &edge.from_entity, &source_id, pin.clone())
                        .await
                }
                None => crate::application_data::GovernedApplicationDataService::new(state)
                    .get(tenant, &edge.from_entity, &source_id)
                    .await
                    .map_err(|error| error.to_string()),
            };
            if let Ok(source_state) = source_state {
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
#[instrument(skip_all, fields(otel.name = "constraint.post_write_invariant_checks", tenant = %tenant, entity_type, entity_id, action_name = labels.0, operation = labels.1))]
pub async fn post_write_invariant_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    labels: (&str, &str),
    fields: &serde_json::Value,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<(), ConstraintViolation> {
    let (action, operation) = labels;
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let start = Instant::now(); // determinism-ok: scoped duration measurement, not simulation-visible state
    let (tenant_name, invariants): (String, Vec<CrossInvariant>) = {
        let registry = state.registry.read().unwrap();
        let config = match schema_pin {
            Some(pin) => {
                registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            }
            None => registry.get_tenant(tenant),
        };
        let Some(tc) = config else {
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
        let Some(assertion) = parse_related_field_assert(&inv.assertion) else {
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

        let target_exists = match schema_pin {
            Some(pin) => {
                state
                    .scoped_reference_target_exists(
                        tenant,
                        &assertion.target_entity,
                        target_id,
                        pin,
                    )
                    .await
            }
            None => {
                state
                    .ensure_entity_loaded(tenant, &assertion.target_entity, target_id)
                    .await
            }
        };
        if !target_exists {
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

        let target_state = match schema_pin {
            Some(pin) => {
                state
                    .get_scoped_entity_state(
                        tenant,
                        &assertion.target_entity,
                        target_id,
                        pin.clone(),
                    )
                    .await
            }
            None => crate::application_data::GovernedApplicationDataService::new(state)
                .get(tenant, &assertion.target_entity, target_id)
                .await
                .map_err(|error| error.to_string()),
        };
        let target_field_value = match target_state {
            Ok(resp) => {
                if assertion.field_name == "status" {
                    resp.state.status.clone()
                } else {
                    let target_fields =
                        serde_json::to_value(&resp.state.fields).unwrap_or_default();
                    let Some(value) =
                        extract_field_as_string(&target_fields, &assertion.field_name)
                    else {
                        tracing::warn!(
                            tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                            target_entity = %assertion.target_entity, target_id,
                            target_field = %assertion.field_name,
                            "constraint violation: target field required by invariant is missing or not a scalar"
                        );
                        state.metrics.record_cross_invariant_violation(
                            &tenant_name,
                            &inv.name,
                            "target_field_missing",
                        );
                        let violation = ConstraintViolation::invariant(
                            &inv.name,
                            format!(
                                "related {}('{}') is missing field '{}' required by invariant",
                                assertion.target_entity, target_id, assertion.field_name
                            ),
                            entity_type,
                            entity_id,
                            operation,
                        );
                        if inv.kind == InvariantKind::Eventual {
                            defer_eventual_invariant(
                                state,
                                &inv,
                                &tenant_name,
                                entity_type,
                                entity_id,
                            );
                            continue;
                        }
                        return Err(violation);
                    };
                    value
                }
            }
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

        let in_list = assertion.values.iter().any(|s| s == &target_field_value);
        let assertion_holds = match assertion.operator {
            CrossInvariantOperator::In => in_list,
            CrossInvariantOperator::NotIn => !in_list,
        };
        if !assertion_holds {
            let (op_str, expectation) = match assertion.operator {
                CrossInvariantOperator::In => ("in", "expected one of"),
                CrossInvariantOperator::NotIn => ("not in", "expected none of"),
            };
            tracing::warn!(
                tenant = %tenant_name, entity_type, entity_id, invariant = %inv.name,
                target_entity = %assertion.target_entity, target_id,
                target_field = %assertion.field_name,
                target_value = %target_field_value,
                operator = op_str,
                expected = ?assertion.values,
                "constraint violation: related entity field mismatch"
            );
            state.metrics.record_cross_invariant_violation(
                &tenant_name,
                &inv.name,
                "status_mismatch",
            );
            let violation = ConstraintViolation::invariant(
                &inv.name,
                format!(
                    "related {}('{}') has {}='{}', {} {:?}",
                    assertion.target_entity,
                    target_id,
                    assertion.field_name,
                    target_field_value,
                    expectation,
                    assertion.values
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

/// Extract a scalar field from a JSON object, coercing numbers/bools to their
/// string representation so they can be compared against the quoted literals
/// in `in`/`not in` assertions.
fn extract_field_as_string(fields: &serde_json::Value, name: &str) -> Option<String> {
    let v = extract_field(fields, name)?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Flatten a `fields` payload so leaf predicates see a single-level object.
///
/// OData write payloads land here with the entity's properties at the top
/// level, but some callers wrap them under a `"fields"` key. Normalise both
/// shapes to the unwrapped form so field_invariant authors don't have to
/// care which handler invoked the check.
fn field_invariant_view(fields: &serde_json::Value) -> serde_json::Value {
    if let Some(inner) = fields.get("fields").and_then(|f| f.as_object()) {
        serde_json::Value::Object(inner.clone())
    } else if fields.is_object() {
        fields.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// Evaluate cross-field invariants declared on an entity's IOA spec against
/// the post-write `initial_fields` payload.
///
/// Runs between [`pre_upsert_relation_checks`] and
/// [`post_write_invariant_checks`] in the write pipeline. Honours the
/// `state.cross_invariant_enforce` feature flag so a single operator control
/// governs all three constraint families. Iteration order follows the order
/// declared in the spec; violations short-circuit on the first failing rule.
#[instrument(skip_all, fields(otel.name = "constraint.pre_upsert_field_invariant_checks", tenant = %tenant, entity_type, entity_id, operation))]
pub async fn pre_upsert_field_invariant_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    fields: &serde_json::Value,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    // Snapshot the field invariants for this (tenant, entity_type). Keep the
    // registry lock scope tight — we don't want to hold it across await points
    // later in the function.
    let invariants: Vec<FieldInvariant> = {
        let registry = state.registry.read().unwrap(); // ci-ok: RwLock read — poisoned lock = prior panic, fail-fast correct
        match schema_pin {
            Some(pin) => registry
                .get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
                .and_then(|config| config.entities.get(entity_type))
                .map(|spec| spec.automaton.field_invariants.clone())
                .unwrap_or_default(),
            None => registry
                .field_invariants_for(tenant, entity_type)
                .unwrap_or_default(),
        }
    };
    if invariants.is_empty() {
        return Ok(());
    }

    let view = field_invariant_view(fields);

    for inv in invariants {
        if inv.passes(&view) {
            continue;
        }
        let message = inv.message.clone().unwrap_or_else(|| {
            format!(
                "field invariant '{}' violated on {}('{}')",
                inv.name, entity_type, entity_id
            )
        });
        state.metrics.record_cross_invariant_violation(
            tenant.as_str(),
            &inv.name,
            "field_invariant",
        );
        tracing::warn!(
            tenant = %tenant, entity_type, entity_id, invariant = %inv.name, operation,
            "constraint violation: field invariant"
        );
        return Err(ConstraintViolation::field_invariant(
            &inv.name,
            message,
            entity_type,
            entity_id,
            operation,
        ));
    }

    Ok(())
}
