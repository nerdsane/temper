//! Policy persistence helpers — bridge between Cedar engine and durable `policies` storage.
//!
//! Canonical entry points:
//!
//! - [`publish_policy_snapshot`]: CAS-publish and activate a complete versioned tenant set.
//! - [`upsert_policy_entries`]: update named rows through that snapshot boundary.
//! - [`install_decision_policy`]: validate, durably persist, and activate exactly one immutable
//!   decision-scoped policy entry. This is the approval security boundary.
//! - [`load_and_activate_tenant_policies`]: read all persisted policy rows for a tenant
//!   from durable storage, combine them, update the in-memory map, and reload the Cedar engine.
//!   Called on tenant registration and at server boot.

use temper_authz::AuthzEngine;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};
use crate::storage::{PolicySnapshot, PolicyStoreRow};

mod decision_policy;
pub use decision_policy::{
    DecisionPolicyInstall, DecisionPolicyReceipt, install_decision_policy,
    rollback_created_decision_policy, verify_active_policy_exactly_once,
};

/// Reserved in-memory identity for immutable configured tenant authority.
pub(crate) const POLICY_BASELINE_ID: &str = "__temper_policy_baseline";

/// Failure to publish one complete tenant policy snapshot.
#[derive(Debug, thiserror::Error)]
pub enum PolicyPublicationError {
    /// Another writer advanced the publication head first.
    #[error("policy publication conflict: expected version {expected}, found {actual}")]
    Conflict { expected: u64, actual: u64 },
    /// The prospective Cedar set is invalid.
    #[error("invalid policy snapshot: {0}")]
    Invalid(String),
    /// Durable storage failed or returned a different committed snapshot.
    #[error("policy snapshot persistence failed: {0}")]
    Persistence(String),
    /// The exact durable snapshot could not be installed in the live engine.
    #[error("policy snapshot activation failed: {0}")]
    Activation(String),
}

/// One named policy entry to create or replace through snapshot publication.
#[derive(Debug, Clone, Copy)]
pub struct PolicyEntryUpsert<'a> {
    /// Stable row identity within the tenant snapshot.
    pub policy_id: &'a str,
    /// Exact Cedar source for this row.
    pub cedar_text: &'a str,
    /// Audited identity responsible for this publication.
    pub created_by: &'a str,
}

fn enabled_named_policies(rows: &[PolicyStoreRow]) -> Vec<(String, String)> {
    rows.iter()
        .filter(|row| row.enabled)
        .map(|row| (row.policy_id.clone(), row.cedar_text.clone()))
        .collect()
}

fn validate_named_policies(rows: &[PolicyStoreRow]) -> Result<(), String> {
    if rows.iter().any(|row| row.policy_id == POLICY_BASELINE_ID) {
        return Err(format!(
            "policy id {POLICY_BASELINE_ID:?} is reserved for configured tenant authority"
        ));
    }
    AuthzEngine::empty()
        .reload_tenant_policies_named("validation", &enabled_named_policies(rows))
        .map_err(|error| format!("decision policy validation failed: {error}"))
}

fn activate_named_policies(
    state: &ServerState,
    tenant: &str,
    rows: &[PolicyStoreRow],
) -> Result<(), String> {
    let named = enabled_named_policies_with_baseline(state, tenant, rows)?;
    state
        .authz
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|error| format!("decision policy activation failed: {error}"))?;
    let active_text = state
        .authz
        .get_tenant_policy_text(tenant)
        .ok_or_else(|| "activated tenant policy set could not be read back".to_string())?;
    state
        .tenant_policies
        .write()
        .map_err(|_| "tenant policy cache lock poisoned".to_string())?
        .insert(tenant.to_string(), active_text);
    Ok(())
}

fn enabled_named_policies_with_baseline(
    state: &ServerState,
    tenant: &str,
    rows: &[PolicyStoreRow],
) -> Result<Vec<(String, String)>, String> {
    let mut named = enabled_named_policies(rows);
    if let Some(baseline) = state
        .tenant_policy_baselines
        .read()
        .map_err(|_| "tenant policy baseline lock poisoned".to_string())?
        .get(tenant)
        .cloned()
    {
        named.push((POLICY_BASELINE_ID.to_string(), baseline));
    }
    Ok(named)
}

fn expected_policy_text_for_snapshot(
    state: &ServerState,
    tenant: &str,
    snapshot: &PolicySnapshot,
) -> Result<Option<String>, String> {
    let named = enabled_named_policies_with_baseline(state, tenant, &snapshot.rows)?;
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|error| format!("decision policy activation preview failed: {error}"))?;
    Ok(engine.get_tenant_policy_text(tenant))
}

/// Install the exact durable snapshot, including an authoritative empty set.
fn activate_policy_snapshot(
    state: &ServerState,
    tenant: &str,
    snapshot: &PolicySnapshot,
) -> Result<bool, String> {
    // Serialize version comparison with engine/cache replacement. Otherwise a
    // delayed v1 reader can overwrite a v2 activation in the same process.
    let mut versions = state
        .tenant_policy_versions
        .write()
        .map_err(|_| "tenant policy version cache lock poisoned".to_string())?;
    if let Some(active) = versions.get(tenant).copied() {
        if active > snapshot.version {
            return Ok(false);
        }
        if active == snapshot.version {
            let active_text = state.authz.get_tenant_policy_text(tenant);
            let expected_text = expected_policy_text_for_snapshot(state, tenant, snapshot)?;
            if active_text == expected_text {
                return Ok(false);
            }
        }
    }
    activate_named_policies(state, tenant, &snapshot.rows)?;
    versions.insert(tenant.to_string(), snapshot.version);
    Ok(true)
}

fn fail_closed_policy_activation(
    state: &ServerState,
    tenant: &str,
    attempted_version: u64,
) -> Result<(), String> {
    let mut versions = state
        .tenant_policy_versions
        .write()
        .map_err(|_| "tenant policy version cache lock poisoned".to_string())?;
    if versions
        .get(tenant)
        .is_some_and(|active| *active > attempted_version)
    {
        return Ok(());
    }
    activate_named_policies(state, tenant, &[])
        .map_err(|error| format!("fail-closed reload failed: {error}"))?;
    // Do not mark the failed durable version active. The next request must
    // retry convergence instead of treating default-deny as synchronized.
    versions.remove(tenant);
    Ok(())
}

/// Remove all mutable tenant authority after a recovery/convergence fault.
///
/// Configured baseline authority remains active. The durable version marker is
/// cleared so a later request retries recovery instead of treating this
/// fail-closed state as synchronized.
pub fn fail_closed_tenant_policies(state: &ServerState, tenant: &str) -> Result<(), String> {
    let mut versions = state
        .tenant_policy_versions
        .write()
        .map_err(|_| "tenant policy version cache lock poisoned".to_string())?;
    activate_named_policies(state, tenant, &[])?;
    versions.remove(tenant);
    Ok(())
}

fn same_policy_snapshot_rows(left: &[PolicyStoreRow], right: &[PolicyStoreRow]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.tenant == right.tenant
                && left.policy_id == right.policy_id
                && left.cedar_text == right.cedar_text
                && left.created_at == right.created_at
                && left.created_by == right.created_by
                && left.enabled == right.enabled
        })
}

/// Validate, CAS-publish, read back, and activate one complete policy set.
pub async fn publish_policy_snapshot(
    state: &ServerState,
    tenant: &str,
    expected_version: u64,
    mut rows: Vec<PolicyStoreRow>,
) -> Result<PolicySnapshot, PolicyPublicationError> {
    rows.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    validate_named_policies(&rows).map_err(PolicyPublicationError::Invalid)?;
    let store = state.policy_store().ok_or_else(|| {
        PolicyPublicationError::Persistence("durable policy store is not configured".to_string())
    })?;
    let committed_version = match store
        .replace_policy_snapshot(tenant, expected_version, rows.clone())
        .await
    {
        Ok(version) => version,
        Err(PersistenceError::ConcurrencyViolation { expected, actual }) => {
            return Err(PolicyPublicationError::Conflict { expected, actual });
        }
        Err(error) => return Err(PolicyPublicationError::Persistence(error.to_string())),
    };
    let committed = store
        .load_policy_snapshot(tenant)
        .await
        .map_err(|error| PolicyPublicationError::Persistence(error.to_string()))?;
    if committed.version != committed_version || !same_policy_snapshot_rows(&rows, &committed.rows)
    {
        return Err(PolicyPublicationError::Persistence(format!(
            "readback did not match committed publication version {committed_version}"
        )));
    }
    if let Err(error) = activate_policy_snapshot(state, tenant, &committed) {
        // The prospective set was parsed before commit, so activation failure
        // is an engine/cache fault. Replace the tenant engine with explicit
        // default-deny rather than continue serving stale authority.
        let fail_closed = fail_closed_policy_activation(state, tenant, committed.version)
            .err()
            .map(|fail_closed_error| format!("; fail-closed reload failed: {fail_closed_error}"))
            .unwrap_or_default();
        return Err(PolicyPublicationError::Activation(format!(
            "{error}{fail_closed}"
        )));
    }
    Ok(committed)
}

fn record_policy_saved(state: &ServerState, tenant: &str, policy_id: &str, created_by: &str) {
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
            policy_id,
            "failed to enqueue policy_saved trajectory entry"
        );
    }
}

/// Upsert named rows through the canonical complete-snapshot CAS boundary.
pub async fn upsert_policy_entries(
    state: &ServerState,
    tenant: &str,
    entries: &[PolicyEntryUpsert<'_>],
) -> Result<bool, String> {
    let store = state
        .policy_store()
        .ok_or_else(|| "durable policy store is not configured".to_string())?;
    let mut ids = std::collections::BTreeSet::new();
    for entry in entries {
        if entry.policy_id.is_empty() || !ids.insert(entry.policy_id) {
            return Err(format!(
                "policy upsert contains an empty or duplicate id {:?}",
                entry.policy_id
            ));
        }
    }
    let created_at = sim_now().to_rfc3339();
    for _attempt in 0..4 {
        let current = store
            .load_policy_snapshot(tenant)
            .await
            .map_err(|error| error.to_string())?;
        let mut prospective = current.rows.clone();
        let mut changed = Vec::new();
        for entry in entries {
            match prospective
                .iter_mut()
                .find(|row| row.policy_id == entry.policy_id)
            {
                Some(row) if row.cedar_text == entry.cedar_text => {}
                Some(row) => {
                    row.cedar_text = entry.cedar_text.to_string();
                    row.created_at = created_at.clone();
                    row.created_by = entry.created_by.to_string();
                    row.enabled = true;
                    changed.push(*entry);
                }
                None => {
                    prospective.push(PolicyStoreRow {
                        tenant: tenant.to_string(),
                        policy_id: entry.policy_id.to_string(),
                        cedar_text: entry.cedar_text.to_string(),
                        policy_hash: String::new(),
                        created_at: created_at.clone(),
                        created_by: entry.created_by.to_string(),
                        enabled: true,
                    });
                    changed.push(*entry);
                }
            }
        }
        if changed.is_empty() {
            load_and_activate_tenant_policies(state, tenant).await?;
            return Ok(false);
        }
        match publish_policy_snapshot(state, tenant, current.version, prospective).await {
            Ok(_) => {
                for entry in changed {
                    record_policy_saved(state, tenant, entry.policy_id, entry.created_by);
                }
                return Ok(true);
            }
            Err(PolicyPublicationError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("policy publication remained contended after retry budget".to_string())
}

/// Load all persisted Cedar policies for a tenant and activate them.
///
/// Reads every row from the `policies` table for `tenant`, concatenates the
/// `cedar_text` values in insertion order, stores the combined text in
/// `state.tenant_policies`, and reloads the Cedar engine.
///
/// Called on tenant registration and during server boot via `recover_cedar_policies`.
/// An empty durable snapshot is authoritative and replaces any prior permit set.
#[instrument(skip_all, fields(tenant, otel.name = "authz.load_and_activate_tenant_policies"))]
pub async fn load_and_activate_tenant_policies(
    state: &ServerState,
    tenant: &str,
) -> Result<u64, String> {
    let Some(store) = state.policy_store() else {
        return Err("durable policy store is not configured".to_string());
    };

    let snapshot = store
        .load_policy_snapshot(tenant)
        .await
        .map_err(|error| format!("failed to load durable policy snapshot: {error}"))?;
    let enabled_count = snapshot.rows.iter().filter(|row| row.enabled).count();
    let activated = activate_policy_snapshot(state, tenant, &snapshot)?;
    let active_version = state
        .tenant_policy_versions
        .read()
        .map_err(|_| "tenant policy version cache lock poisoned".to_string())?
        .get(tenant)
        .copied()
        .ok_or_else(|| "policy activation completed without a version marker".to_string())?;

    tracing::info!(
        tenant,
        durable_version = snapshot.version,
        active_version,
        total = snapshot.rows.len(),
        enabled = enabled_count,
        activated,
        "Cedar policy snapshot activated from durable storage"
    );
    Ok(active_version)
}

/// Converge one process replica to the latest durable tenant policy snapshot.
///
/// Call this at request ingress before any authorization decision. The
/// version-guarded activation makes delayed readers harmless while the durable
/// load ensures a replica observes publications made by another process.
pub async fn refresh_policy_snapshot_if_stale(
    state: &ServerState,
    tenant: &str,
) -> Result<u64, String> {
    load_and_activate_tenant_policies(state, tenant).await
}

/// Recover one tenant from the canonical snapshot, migrating one legacy blob if needed.
pub async fn recover_policy_snapshot(
    state: &ServerState,
    tenant: &str,
    legacy_policy_text: Option<&str>,
) -> Result<u64, String> {
    let store = state
        .policy_store()
        .ok_or_else(|| "durable policy store is not configured".to_string())?;
    let snapshot = store
        .load_policy_snapshot(tenant)
        .await
        .map_err(|error| error.to_string())?;
    if snapshot.version == 0
        && snapshot.rows.is_empty()
        && let Some(legacy) = legacy_policy_text.filter(|text| !text.trim().is_empty())
    {
        upsert_policy_entries(
            state,
            tenant,
            &[PolicyEntryUpsert {
                policy_id: "migrated-legacy",
                cedar_text: legacy,
                created_by: "startup-migration",
            }],
        )
        .await?;
    }
    load_and_activate_tenant_policies(state, tenant).await
}
