//! Field invariant enforcement on the OData write path.

use tracing::instrument;

use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;

use super::constraints::ConstraintViolation;
use crate::state::ServerState;

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
/// Runs between [`super::constraints::pre_upsert_relation_checks`] and
/// [`super::constraints::post_write_invariant_checks`] in the write pipeline.
/// Honours the `state.cross_invariant_enforce` feature flag so a single
/// operator control governs all three constraint families. Iteration order
/// follows the order declared in the spec; violations short-circuit on the
/// first failing rule.
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
    let invariants: Vec<FieldInvariant> = match state.registry.read() {
        Ok(registry) => registry
            .field_invariants_for(tenant, entity_type)
            .unwrap_or_default(),
        Err(_) => return Ok(()), // poisoned lock — prior panic, skip gracefully
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
