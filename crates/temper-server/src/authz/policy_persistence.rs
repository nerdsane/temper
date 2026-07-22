//! Policy persistence helpers — bridge between Cedar engine and durable `policies` storage.
//!
//! Two entry points:
//!
//! - [`persist_and_activate_policy`]: write a new/updated policy entry to durable storage (hash-gated)
//!   and log a trajectory entry if the content changed.  Cedar engine reload is the
//!   **caller's responsibility** — callers must invoke `validate_and_reload_policies` before
//!   calling this function.
//! - [`load_and_activate_tenant_policies`]: read all persisted policy rows for a tenant
//!   from durable storage, combine them, update the in-memory map, and reload the Cedar engine.
//!   Called on tenant registration and at server boot.

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};
use crate::storage::{PolicyGenerationWrite, PolicyStore, PolicyStoreRow};

/// Load the canonical granular generation, migrating a legacy aggregate into
/// its reserved `primary` row before the first granular mutation.
///
/// A crash after the seed but before the requested mutation is safe: restart
/// sees the same legacy grants in `primary`, and the caller can retry the
/// mutation. No first-granular writer may make the legacy generation vanish.
pub(crate) async fn load_or_seed_policy_generation(
    state: &ServerState,
    store: &dyn PolicyStore,
    tenant: &str,
) -> Result<Vec<PolicyStoreRow>, String> {
    let rows = store.load_policies_for_tenant(tenant).await?;
    if !rows.is_empty() {
        // Granular rows are the canonical generation. The aggregate blob and
        // in-memory text are compatibility projections and must never be promoted
        // once any owned row exists, even when their deduplicated text differs.
        return Ok(rows);
    }

    let durable_legacy = store.load_policy_compatibility_text(tenant).await?;
    let legacy = durable_legacy.or_else(|| {
        state
            .tenant_policies
            .read()
            .ok()
            .and_then(|policies| policies.get(tenant).cloned())
            .or_else(|| state.authz.get_tenant_policy_text(tenant))
    });
    let Some(legacy) = legacy.filter(|policy| !policy.trim().is_empty()) else {
        return Ok(rows);
    };

    store
        .save_policy(tenant, "primary", legacy.trim(), "legacy-migration")
        .await?;
    let migrated = store.load_policies_for_tenant(tenant).await?;
    if !migrated.iter().any(|row| row.policy_id == "primary") {
        return Err(format!(
            "legacy Cedar generation for tenant '{tenant}' was not durably seeded"
        ));
    }
    Ok(migrated)
}

/// Persist a complete granular generation and its compatibility projection in
/// one backend transaction, then emit the existing policy-change trajectory.
pub(crate) async fn persist_complete_policy_generation(
    state: &ServerState,
    tenant: &str,
    entries: &[PolicyGenerationWrite],
    changed_policy_id: &str,
    created_by: &str,
) -> Result<(), String> {
    let Some(store) = state.policy_store() else {
        return Err("durable policy store not configured".to_string());
    };
    let mut entries = entries.to_vec();
    entries.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    let compatibility_text = entries
        .iter()
        .filter(|entry| entry.enabled)
        .map(|entry| entry.cedar_text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    store
        .replace_policy_generation(tenant, &entries, &compatibility_text)
        .await
        .map_err(|error| {
            tracing::warn!(
                %error,
                tenant,
                policy_id = changed_policy_id,
                "failed to persist complete Cedar policy generation"
            );
            error
        })?;

    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: "_cedar".to_string(),
        entity_id: tenant.to_string(),
        action: "policy_saved".to_string(),
        success: true,
        from_status: None,
        to_status: None,
        error: None,
        agent_id: Some(created_by.to_string()),
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Platform),
        spec_governed: Some(false),
        agent_type: None,
        request_body: None,
        intent: None,
        matched_policy_ids: None,
    };
    if !state.enqueue_trajectory_entry(entry) {
        tracing::warn!(
            tenant,
            policy_id = changed_policy_id,
            "failed to enqueue policy_saved trajectory entry"
        );
    }
    tracing::info!(
        tenant,
        policy_id = changed_policy_id,
        created_by,
        "Cedar policy change logged"
    );
    Ok(())
}

/// Persist a Cedar policy entry and log a trajectory entry on change.
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
/// `false` when the hash matched and no write was needed. Persistence absence
/// and write failures are explicit: policy-generation callers must not publish
/// a live Cedar generation when its durable half did not commit.
#[instrument(skip_all, fields(tenant, policy_id, otel.name = "authz.persist_and_activate_policy"))]
pub async fn persist_and_activate_policy(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
) -> Result<bool, String> {
    let Some(store) = state.policy_store() else {
        return Err("durable policy store not configured".to_string());
    };
    let rows = load_or_seed_policy_generation(state, store.as_ref(), tenant).await?;
    let mut entries = rows
        .into_iter()
        .map(|row| PolicyGenerationWrite {
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            enabled: row.enabled,
            created_by: row.created_by,
        })
        .collect::<Vec<_>>();
    let existing = entries
        .iter()
        .position(|entry| entry.policy_id == policy_id);
    if existing
        .is_some_and(|index| entries[index].cedar_text == cedar_text && entries[index].enabled)
    {
        return Ok(false);
    }
    if let Some(index) = existing {
        entries[index].cedar_text = cedar_text.to_string();
        entries[index].enabled = true;
        entries[index].created_by = created_by.to_string();
    } else {
        entries.push(PolicyGenerationWrite {
            policy_id: policy_id.to_string(),
            cedar_text: cedar_text.to_string(),
            enabled: true,
            created_by: created_by.to_string(),
        });
    }
    persist_complete_policy_generation(state, tenant, &entries, policy_id, created_by).await?;
    Ok(true)
}

/// Publish one generated policy as a complete durable/live tenant generation.
///
/// This is the non-HTTP owner used by spec-driven governance effects. API
/// handlers perform their authorization inside an equivalent writer before
/// calling their own mutation path; both converge on the same `policy_id`, so
/// retries are idempotent and cannot append duplicate Cedar statements.
pub async fn publish_policy_entry_generation(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
) -> Result<(), String> {
    let tenant_id = TenantId::new(tenant);
    let mut guard = state.begin_spec_publication(&tenant_id).await?;
    let store = state
        .policy_store()
        .ok_or_else(|| "durable policy store not configured".to_string())?;
    let mut entries = load_or_seed_policy_generation(state, store.as_ref(), tenant)
        .await
        .map_err(|error| format!("failed to load durable policy generation: {error}"))?
        .into_iter()
        .map(|row| PolicyGenerationWrite {
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            enabled: row.enabled,
            created_by: row.created_by,
        })
        .collect::<Vec<_>>();
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.policy_id == policy_id)
    {
        entry.cedar_text = cedar_text.to_string();
        entry.enabled = true;
        entry.created_by = created_by.to_string();
    } else {
        entries.push(PolicyGenerationWrite {
            policy_id: policy_id.to_string(),
            cedar_text: cedar_text.to_string(),
            enabled: true,
            created_by: created_by.to_string(),
        });
    }
    entries.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    let named = entries
        .iter()
        .filter(|entry| entry.enabled)
        .map(|entry| (entry.policy_id.clone(), entry.cedar_text.clone()))
        .collect::<Vec<_>>();
    let mut validation_text = String::new();
    for (_, text) in &named {
        if !validation_text.is_empty() {
            validation_text.push('\n');
        }
        validation_text.push_str(text);
    }
    temper_authz::AuthzEngine::new(&validation_text)
        .map_err(|error| format!("generated policy would invalidate tenant policy set: {error}"))?;
    let mut intent_components = vec![
        ("policy-id".to_string(), policy_id.as_bytes().to_vec()),
        ("cedar-text".to_string(), cedar_text.as_bytes().to_vec()),
        ("created-by".to_string(), created_by.as_bytes().to_vec()),
    ];
    intent_components.extend(entries.iter().map(|entry| {
        (
            format!("entry:{}", entry.policy_id),
            [entry.cedar_text.as_bytes(), &[u8::from(entry.enabled)]].concat(),
        )
    }));
    let intent = ServerState::spec_publication_intent(
        "generated-policy-upsert-v1",
        intent_components
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_slice())),
    );
    state.arm_spec_publication(&mut guard, &tenant_id, &intent)?;
    persist_complete_policy_generation(state, tenant, &entries, policy_id, created_by).await?;
    state
        .authz
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|error| format!("failed to activate generated policy: {error}"))?;
    let combined = state
        .authz
        .get_tenant_policy_text(tenant)
        .unwrap_or_default();
    state
        .tenant_policies
        .write()
        .map_err(|error| format!("policy compatibility cache lock poisoned: {error}"))?
        .insert(tenant.to_string(), combined);
    state.complete_spec_publication_retry(&mut guard, &tenant_id)?;
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);
    Ok(())
}

/// Load all persisted Cedar policies for a tenant and activate them.
///
/// Reads every row from the `policies` table for `tenant`, concatenates the
/// `cedar_text` values in insertion order, stores the combined text in
/// `state.tenant_policies`, and reloads the Cedar engine.
///
/// Called on tenant registration and during server boot via `recover_cedar_policies`.
/// Silently degrades (logs a warning) if durable storage is unavailable or the table is empty.
#[instrument(skip_all, fields(tenant, otel.name = "authz.load_and_activate_tenant_policies"))]
pub async fn load_and_activate_tenant_policies(state: &ServerState, tenant: &str) {
    let Some(store) = state.policy_store() else {
        return;
    };

    let rows = match load_or_seed_policy_generation(state, store.as_ref(), tenant).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                tenant,
                "failed to load or migrate Cedar policies from durable storage"
            );
            return;
        }
    };

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
            "failed to reload Cedar engine after loading durable policies"
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
        "Cedar policies activated from durable `policies` table"
    );
}
