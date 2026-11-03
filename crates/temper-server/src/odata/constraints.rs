//! Cross-entity relation and invariant enforcement.

use std::collections::{BTreeMap, BTreeSet};
mod delete_plan;
mod delete_references;
mod related_record;

use related_record::read_related_record;

pub use delete_plan::{DeletePlan, plan_delete};

use std::time::Instant; // determinism-ok: scoped duration measurement only

use futures_util::FutureExt;
use tracing::instrument;

use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;
use temper_spec::cross_invariant::{
    CrossInvariant, CrossInvariantOperator, InvariantKind, parse_related_field_assert,
};

use crate::entity_actor::EntityState;
use crate::odata::read_defaults::ReadDefaults;
use crate::registry::RelationEdge;
use crate::state::ServerState;
use crate::state::write_record::RecordWrite;

/// Records a write brings into being after it commits, read by the checks in
/// place of the store: the children a parent's action spawns, which the
/// kernel creates once the parent commits.
#[derive(Debug, Default)]
pub(crate) struct PendingRecords(BTreeMap<(String, String), EntityState>);

impl PendingRecords {
    /// Add the record as it will stand once the write commits.
    pub(crate) fn insert(&mut self, record: EntityState) {
        self.0.insert(
            (record.entity_type.clone(), record.entity_id.clone()),
            record,
        );
    }

    /// Read a child as the preceding spawn initializer leaves it.
    pub(crate) fn get(&self, entity_type: &str, entity_id: &str) -> Option<&EntityState> {
        self.0
            .get(&(entity_type.to_string(), entity_id.to_string()))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintViolationType {
    RelationIntegrity,
    /// A related or referencing record could not be checked.
    RelationUnverified,
    CrossInvariant,
    FieldInvariant,
    /// A value the checks read could not be read back from where an earlier
    /// write stored it, so the record cannot be checked and is not written.
    StoredValueUnreadable,
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

    fn relation_unverified(
        message: impl Into<String>,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
    ) -> Self {
        Self {
            violation_type: ConstraintViolationType::RelationUnverified,
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

    /// The record's checks read a stored value that could not be read.
    pub(crate) fn stored_value_unreadable(
        entity_type: &str,
        entity_id: &str,
        operation: &str,
    ) -> Self {
        Self {
            violation_type: ConstraintViolationType::StoredValueUnreadable,
            invariant: None,
            message: format!(
                "a stored value of {entity_type}('{entity_id}') that this write is checked against could not be read; retry the write"
            ),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            operation: operation.to_string(),
        }
    }
}

/// Check FK integrity for create/update style writes. A target in `pending`
/// exists: the write creates it once it commits.
#[instrument(skip_all, fields(otel.name = "constraint.pre_upsert_relation_checks", tenant = %tenant, entity_type, entity_id, operation))]
pub(crate) async fn pre_upsert_relation_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    fields: &serde_json::Value,
    pending: &PendingRecords,
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
        let target = read_related_record(state, tenant, &edge.to_entity, target_id, pending)
            .await
            .map_err(|error| {
                state.metrics.record_relation_integrity_violation(
                    &tenant_name,
                    entity_type,
                    operation,
                );
                ConstraintViolation::relation_unverified(
                    format!(
                        "failed to read relation target {}('{}'): {error}",
                        edge.to_entity, target_id
                    ),
                    entity_type,
                    entity_id,
                    operation,
                )
            })?;
        if target.is_none() {
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

/// Evaluate cross-entity invariants on the stored record and its pending children.
#[instrument(skip_all, fields(otel.name = "constraint.post_write_invariant_checks", tenant = %write.tenant, entity_type = write.entity_type, entity_id = write.entity_id, action_name = write.action, operation = write.operation))]
pub(crate) async fn post_write_invariant_checks(
    state: &ServerState,
    write: RecordWrite<'_>,
    fields: &serde_json::Value,
    pending: &PendingRecords,
) -> Result<(), ConstraintViolation> {
    let RecordWrite {
        tenant,
        entity_type,
        entity_id,
        action,
        operation,
        ..
    } = write;
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

        let target =
            match read_related_record(state, tenant, &assertion.target_entity, target_id, pending)
                .await
            {
                Ok(Some(target)) => Ok(target),
                Err(error) => Err(error),
                Ok(None) => {
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
            };
        // The related record's field as its value: one an earlier write
        // stored out of line is read back, never compared as a reference.
        let target = match target {
            Ok(mut target) if assertion.field_name != "status" => {
                let names = BTreeSet::from([assertion.field_name.clone()]);
                let source = crate::blobs::BlobReadSource::Tenant { state, tenant };
                // Boxed: the blob read's future is too deep for the write
                // paths' `Send` checks.
                crate::blobs::resolve_stored_fields(&source, &target.fields, &names)
                    .boxed()
                    .await
                    .map(|resolved| {
                        if let Some(resolved) = resolved {
                            target.fields = resolved;
                        }
                        target
                    })
            }
            other => other,
        };
        let target_field_value = match target {
            Ok(target) => {
                if assertion.field_name == "status" {
                    target.status
                } else {
                    let target_fields =
                        ReadDefaults::for_entity(state, tenant, &assertion.target_entity)
                            .fields_as_read(&target.fields);
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

/// Whether a cross-invariant's `on` trigger (`Entity.Action` or `Entity.*`)
/// selects a write of `action` on `entity_type`.
pub(crate) fn trigger_matches(on: &str, entity_type: &str, action: &str) -> bool {
    let Some((entity, action_sel)) = on.split_once('.') else {
        return false;
    };
    if entity.trim() != entity_type {
        return false;
    }
    let action_sel = action_sel.trim();
    action_sel == "*" || action_sel == action
}

/// A field of the record being checked. The record is the entity's fields as
/// they will be stored, so a field is read only at the top level.
fn extract_field<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    fields.get(name)
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

/// The object field invariants are evaluated on: the record as it will be
/// stored, with its properties (including `Status`) at the top level.
fn field_invariant_view(fields: &serde_json::Value) -> serde_json::Value {
    if fields.is_object() {
        fields.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// Evaluate cross-field invariants declared on an entity's IOA spec against
/// the record the write will store.
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
        registry
            .field_invariants_for(tenant, entity_type)
            .unwrap_or_default()
    };
    if invariants.is_empty() {
        return Ok(());
    }

    // A variable the record stores no value for reads as its declared
    // initial value, as it does in every guard.
    let view = ReadDefaults::for_entity(state, tenant, entity_type)
        .fields_as_read(&field_invariant_view(fields));

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
