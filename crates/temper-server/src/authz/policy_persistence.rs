//! Policy persistence helpers — bridge between Cedar engine and Turso `policies` table.
//!
//! Two entry points:
//!
//! - [`persist_and_activate_policy`]: write a new/updated policy entry to Turso (hash-gated)
//!   and log a trajectory entry if the content changed.  Cedar engine reload is the
//!   **caller's responsibility** — callers must invoke `validate_and_reload_policies` before
//!   calling this function.
//! - [`load_and_activate_tenant_policies`]: read all persisted policy rows for a tenant
//!   from Turso, combine them, update the in-memory map, and reload the Cedar engine.
//!   Called on tenant registration and at server boot.

use temper_runtime::scheduler::sim_now;
use temper_store_turso::{TursoEventStore, TursoTrajectoryInsert};
use tracing::instrument;

use crate::state::ServerState;

/// Persist a Cedar policy entry to Turso and log a trajectory entry on change.
///
/// Uses SHA-256 hash comparison to skip redundant writes.  When the content
/// changes, a [`TrajectoryEntry`] is logged with `action = "policy_saved"` and
/// `source = "Platform"` so the Evolution Engine can correlate policy changes
/// with subsequent authorization outcomes.
///
/// **Cedar engine reload is the caller's responsibility.**  Callers must invoke
/// `validate_and_reload_policies` (or equivalent) before calling this function.
/// This function only handles durable persistence and observability.
///
/// Returns `true` if the policy was written (content changed or new entry),
/// `false` when the hash matched and no write was needed.
/// Returns `false` silently (with a `tracing::debug` log) when no Turso store
/// is configured — callers that need to know should check `state.platform_persistent_store()`.
#[instrument(skip_all, fields(tenant, policy_id, otel.name = "authz.persist_and_activate_policy"))]
pub async fn persist_and_activate_policy(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
) -> bool {
    let Some(turso) = state.persistent_store_for_tenant(tenant).await else {
        tracing::debug!(
            tenant,
            policy_id,
            "Turso not configured; skipping policy persistence"
        );
        return false;
    };

    let changed = match turso
        .save_policy(tenant, policy_id, cedar_text, created_by)
        .await
    {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!(
                error = %e,
                tenant,
                policy_id,
                "failed to persist Cedar policy to Turso"
            );
            return false;
        }
    };

    if changed {
        // Log a trajectory entry so the policy change is observable in the
        // Evolution Engine dashboard and trajectory analytics.
        let now = sim_now().to_rfc3339();
        if let Err(e) = turso
            .persist_trajectory(TursoTrajectoryInsert {
                tenant,
                entity_type: "_cedar",
                entity_id: tenant,
                action: "policy_saved",
                success: true,
                from_status: None,
                to_status: None,
                error: None,
                agent_id: Some(created_by),
                session_id: None,
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some("Platform"),
                spec_governed: Some(false),
                created_at: &now,
                request_body: None,
                intent: None,
            })
            .await
        {
            tracing::warn!(
                error = %e,
                tenant,
                policy_id,
                "failed to log policy_saved trajectory entry"
            );
        }
        tracing::info!(tenant, policy_id, created_by, "Cedar policy change logged");
    }

    changed
}

/// Load all persisted Cedar policies for a tenant from Turso and activate them.
///
/// Reads every row from the `policies` table for `tenant`, concatenates the
/// `cedar_text` values in insertion order, stores the combined text in
/// `state.tenant_policies`, and reloads the Cedar engine.
///
/// Called on tenant registration and during server boot via `recover_cedar_policies`.
/// Silently degrades (logs a warning) if Turso is unavailable or the table is empty.
#[instrument(skip_all, fields(tenant, otel.name = "authz.load_and_activate_tenant_policies"))]
pub async fn load_and_activate_tenant_policies(
    state: &ServerState,
    tenant: &str,
    turso: &TursoEventStore,
) {
    let rows = match turso.load_policies_for_tenant(tenant).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                tenant,
                "failed to load Cedar policies from `policies` table"
            );
            return;
        }
    };

    if rows.is_empty() {
        return;
    }

    // Build named policy entries for per-policy PolicyId assignment.
    let enabled_count = rows.iter().filter(|r| r.enabled).count();
    let named_policies: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.enabled)
        .map(|r| (r.policy_id.clone(), r.cedar_text.clone()))
        .collect();

    // Load into the per-tenant Cedar engine with meaningful PolicyIds
    // (e.g., "default:os-app:project-management:2" instead of "policy0").
    if let Err(e) = state
        .authz
        .reload_tenant_policies_named(tenant, &named_policies)
    {
        tracing::warn!(
            error = %e,
            tenant,
            "failed to reload Cedar engine after loading policies from Turso"
        );
        return;
    }

    // Also update the in-memory text cache for backward compat (GET endpoint,
    // prospective text building).
    let combined_for_tenant = state
        .authz
        .get_tenant_policy_text(tenant)
        .unwrap_or_default();
    if let Ok(mut policies) = state.tenant_policies.write() {
        policies.insert(tenant.to_string(), combined_for_tenant);
    }

    tracing::info!(
        tenant,
        total = rows.len(),
        enabled = enabled_count,
        "Cedar policies activated from Turso `policies` table"
    );
}
