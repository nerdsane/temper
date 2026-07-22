//! Durable Cedar policy-generation publication helpers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_runtime::tenant::TenantId;

use crate::authz::policy_persistence::load_or_seed_policy_generation;
use crate::authz::policy_persistence::persist_complete_policy_generation;
use crate::state::{ServerState, SpecPublicationGuard};
use crate::storage::{PolicyGenerationWrite, PolicyStore, PolicyStoreRow};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PolicyGenerationEntry {
    pub(super) policy_id: String,
    pub(super) cedar_text: String,
    pub(super) enabled: bool,
    pub(super) created_by: String,
}

impl From<&PolicyStoreRow> for PolicyGenerationEntry {
    fn from(row: &PolicyStoreRow) -> Self {
        Self {
            policy_id: row.policy_id.clone(),
            cedar_text: row.cedar_text.clone(),
            enabled: row.enabled,
            created_by: row.created_by.clone(),
        }
    }
}

pub(super) enum PolicyUpsert {
    Replace(String),
    AppendRule(String),
}

pub(super) async fn begin_policy_generation_mutation(
    state: &ServerState,
    tenant: &str,
    expected_generation: Option<u64>,
    auth_headers: Option<&HeaderMap>,
) -> Result<SpecPublicationGuard, axum::response::Response> {
    let tenant_id = TenantId::new(tenant);
    let guard = match expected_generation {
        Some(expected_generation) => {
            state
                .begin_spec_publication_after_drain(&tenant_id, expected_generation)
                .await
        }
        None => state.begin_spec_publication(&tenant_id).await,
    }
    .map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Tenant runtime generation is busy: {error}"),
        )
            .into_response()
    })?;
    if let Some(headers) = auth_headers
        && let Some(response) = super::super::require_policy_auth(state, headers, tenant).await
    {
        return Err(response);
    }
    Ok(guard)
}

pub(super) async fn begin_policy_generation_read(
    state: &ServerState,
    tenant: &str,
) -> Result<tokio::sync::OwnedRwLockReadGuard<()>, axum::response::Response> {
    let tenant_id = TenantId::new(tenant);
    if state.spec_publication_gated(&tenant_id) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Tenant runtime generation has an unresolved publication",
        )
            .into_response());
    }
    let guard = state
        .try_begin_tenant_request(&tenant_id)
        .await
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Tenant runtime generation is busy",
            )
                .into_response()
        })?;
    if state.spec_publication_gated(&tenant_id) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Tenant runtime generation has an unresolved publication",
        )
            .into_response());
    }
    Ok(guard)
}

pub(super) async fn begin_durable_policy_generation(
    state: &ServerState,
    tenant: &str,
    expected_generation: Option<u64>,
    auth_headers: Option<&HeaderMap>,
) -> Result<
    (
        SpecPublicationGuard,
        std::sync::Arc<dyn PolicyStore>,
        Vec<PolicyGenerationEntry>,
    ),
    axum::response::Response,
> {
    let guard =
        begin_policy_generation_mutation(state, tenant, expected_generation, auth_headers).await?;
    let store = state.policy_store().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response()
    })?;
    let rows = load_or_seed_policy_generation(state, store.as_ref(), tenant)
        .await
        .map_err(|error| {
            tracing::warn!(%error, tenant, "failed to load durable policy generation");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load policies: {error}"),
            )
                .into_response()
        })?;
    let mut entries = rows
        .iter()
        .map(PolicyGenerationEntry::from)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    Ok((guard, store, entries))
}

pub(super) fn enabled_named_policies(entries: &[PolicyGenerationEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|entry| entry.enabled)
        .map(|entry| (entry.policy_id.clone(), entry.cedar_text.clone()))
        .collect()
}

pub(super) fn policy_generation_writes(
    entries: &[PolicyGenerationEntry],
) -> Vec<PolicyGenerationWrite> {
    entries
        .iter()
        .map(|entry| PolicyGenerationWrite {
            policy_id: entry.policy_id.clone(),
            cedar_text: entry.cedar_text.clone(),
            enabled: entry.enabled,
            created_by: entry.created_by.clone(),
        })
        .collect()
}

#[expect(
    clippy::result_large_err,
    reason = "policy helpers return the fully constructed HTTP rejection response"
)]
pub(super) fn validate_policy_generation(
    tenant: &str,
    entries: &[PolicyGenerationEntry],
) -> Result<(), axum::response::Response> {
    let named = enabled_named_policies(entries);
    temper_authz::AuthzEngine::empty()
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|error| {
            tracing::warn!(%error, tenant, "policy validation failed");
            (
                StatusCode::BAD_REQUEST,
                format!("Policy validation failed: {error}"),
            )
                .into_response()
        })
}

pub(super) fn policy_generation_intent(
    kind: &str,
    entries: &[PolicyGenerationEntry],
    request_components: &[(&str, &[u8])],
) -> String {
    let mut components = request_components
        .iter()
        .map(|(name, value)| (format!("request:{name}"), value.to_vec()))
        .collect::<Vec<_>>();
    for entry in entries {
        components.push((
            format!("policy:{}:text", entry.policy_id),
            entry.cedar_text.as_bytes().to_vec(),
        ));
        components.push((
            format!("policy:{}:enabled", entry.policy_id),
            vec![u8::from(entry.enabled)],
        ));
    }
    ServerState::spec_publication_intent(
        kind,
        components
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_slice())),
    )
}

#[expect(
    clippy::result_large_err,
    reason = "policy helpers return the fully constructed HTTP rejection response"
)]
pub(super) fn arm_policy_generation(
    state: &ServerState,
    guard: &mut SpecPublicationGuard,
    tenant: &str,
    intent: &str,
) -> Result<(), axum::response::Response> {
    let tenant_id = TenantId::new(tenant);
    state
        .arm_spec_publication(guard, &tenant_id, intent)
        .map_err(|error| {
            tracing::warn!(%error, tenant, "failed to arm policy generation");
            (StatusCode::CONFLICT, error).into_response()
        })
}

#[expect(
    clippy::result_large_err,
    reason = "policy helpers return the fully constructed HTTP rejection response"
)]
pub(super) fn activate_policy_generation(
    state: &ServerState,
    tenant: &str,
    entries: &[PolicyGenerationEntry],
    guard: &mut SpecPublicationGuard,
) -> Result<(), axum::response::Response> {
    let named = enabled_named_policies(entries);
    state
        .authz
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|error| {
            tracing::error!(%error, tenant, "durable policy generation could not be activated");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Policy generation activation failed: {error}"),
            )
                .into_response()
        })?;
    let combined = state
        .authz
        .get_tenant_policy_text(tenant)
        .unwrap_or_default();
    state
        .tenant_policies
        .write()
        .map_err(|error| {
            tracing::error!(%error, tenant, "policy compatibility cache lock poisoned");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Policy generation activation failed",
            )
                .into_response()
        })?
        .insert(tenant.to_string(), combined);
    state
        .complete_spec_publication_retry(guard, &TenantId::new(tenant))
        .map_err(|error| {
            tracing::error!(%error, tenant, "failed to release completed policy generation");
            (StatusCode::SERVICE_UNAVAILABLE, error).into_response()
        })?;
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "policy helpers return the fully constructed HTTP rejection response"
)]
pub(super) fn publish_memory_policy_generation(
    state: &ServerState,
    tenant: &str,
    policy_text: &str,
    kind: &str,
    guard: &mut SpecPublicationGuard,
) -> Result<(), axum::response::Response> {
    temper_authz::AuthzEngine::new(policy_text).map_err(|error| {
        tracing::warn!(%error, tenant, "policy validation failed");
        (
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {error}"),
        )
            .into_response()
    })?;
    let intent =
        ServerState::spec_publication_intent(kind, [("policy-text", policy_text.as_bytes())]);
    arm_policy_generation(state, guard, tenant, &intent)?;
    super::super::validate_and_reload_policies(state, tenant, policy_text)?;
    state
        .tenant_policies
        .write()
        .map_err(|error| {
            tracing::error!(%error, tenant, "policy compatibility cache lock poisoned");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Policy generation activation failed",
            )
                .into_response()
        })?
        .insert(tenant.to_string(), policy_text.to_string());
    state
        .complete_spec_publication_retry(guard, &TenantId::new(tenant))
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error).into_response())
}

pub(super) async fn publish_policy_upsert_mode(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    mutation: PolicyUpsert,
    created_by: &str,
    expected_generation: Option<u64>,
    auth_headers: Option<&HeaderMap>,
) -> Result<(), axum::response::Response> {
    let (mut guard, _store, mut entries) =
        begin_durable_policy_generation(state, tenant, expected_generation, auth_headers).await?;
    let existing = entries
        .iter()
        .position(|entry| entry.policy_id == policy_id);
    let existing_text = existing
        .map(|index| entries[index].cedar_text.as_str())
        .unwrap_or_default();
    let cedar_text = match mutation {
        PolicyUpsert::Replace(text) => text,
        PolicyUpsert::AppendRule(rule) if existing_text.is_empty() => rule,
        PolicyUpsert::AppendRule(rule)
            if existing_text == rule || existing_text.ends_with(&format!("\n{rule}")) =>
        {
            existing_text.to_string()
        }
        PolicyUpsert::AppendRule(rule) => format!("{existing_text}\n{rule}"),
    };
    if let Some(index) = existing {
        entries[index].cedar_text.clone_from(&cedar_text);
        entries[index].enabled = true;
    } else {
        entries.push(PolicyGenerationEntry {
            policy_id: policy_id.to_string(),
            cedar_text: cedar_text.clone(),
            enabled: true,
            created_by: created_by.to_string(),
        });
    }
    if let Some(index) = existing {
        entries[index].created_by = created_by.to_string();
    }
    entries.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
    validate_policy_generation(tenant, &entries)?;
    let intent = policy_generation_intent(
        "direct-policy-upsert-v1",
        &entries,
        &[
            ("policy-id", policy_id.as_bytes()),
            ("cedar-text", cedar_text.as_bytes()),
            ("created-by", created_by.as_bytes()),
        ],
    );
    arm_policy_generation(state, &mut guard, tenant, &intent)?;
    let writes = policy_generation_writes(&entries);
    persist_complete_policy_generation(state, tenant, &writes, policy_id, created_by)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to persist policy: {error}"),
            )
                .into_response()
        })?;
    activate_policy_generation(state, tenant, &entries, &mut guard)
}

pub(in crate::api) async fn publish_policy_replace_all(
    state: &ServerState,
    tenant: &str,
    cedar_text: &str,
    created_by: &str,
    expected_generation: Option<u64>,
    auth_headers: Option<&HeaderMap>,
) -> Result<(), axum::response::Response> {
    let (mut guard, _store, _existing) =
        begin_durable_policy_generation(state, tenant, expected_generation, auth_headers).await?;
    let entries = vec![PolicyGenerationEntry {
        policy_id: "primary".to_string(),
        cedar_text: cedar_text.to_string(),
        enabled: true,
        created_by: created_by.to_string(),
    }];
    validate_policy_generation(tenant, &entries)?;
    let intent = policy_generation_intent(
        "direct-policy-replace-all-v1",
        &entries,
        &[
            ("cedar-text", cedar_text.as_bytes()),
            ("created-by", created_by.as_bytes()),
        ],
    );
    arm_policy_generation(state, &mut guard, tenant, &intent)?;
    persist_complete_policy_generation(
        state,
        tenant,
        &policy_generation_writes(&entries),
        "primary",
        created_by,
    )
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist policy generation: {error}"),
        )
            .into_response()
    })?;
    activate_policy_generation(state, tenant, &entries, &mut guard)
}

pub(in crate::api) async fn publish_policy_upsert(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
    expected_generation: Option<u64>,
    auth_headers: Option<&HeaderMap>,
) -> Result<(), axum::response::Response> {
    publish_policy_upsert_mode(
        state,
        tenant,
        policy_id,
        PolicyUpsert::Replace(cedar_text.to_string()),
        created_by,
        expected_generation,
        auth_headers,
    )
    .await
}
