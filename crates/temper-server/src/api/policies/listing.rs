//! Stable-generation policy listing endpoints.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::instrument;

use super::{PolicyAuthed, ServerState, begin_policy_generation_read};
use crate::storage::PolicyStoreRow;

fn policy_source(policy_id: &str) -> &'static str {
    if policy_id.starts_with("os-app:") {
        "os-app"
    } else if policy_id.starts_with("decision:") {
        "decision"
    } else if policy_id == "migrated-legacy" {
        "migrated-legacy"
    } else {
        "manual"
    }
}

fn policy_row_to_json(row: &PolicyStoreRow) -> serde_json::Value {
    serde_json::json!({
        "tenant": row.tenant,
        "policy_id": row.policy_id,
        "cedar_text": row.cedar_text,
        "enabled": row.enabled,
        "policy_hash": row.policy_hash,
        "created_at": row.created_at,
        "created_by": row.created_by,
        "source": policy_source(&row.policy_id),
    })
}

/// GET /api/tenants/{tenant}/policies/list — list individual policy entries.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies/list"))]
pub(crate) async fn handle_list_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
) -> impl IntoResponse {
    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    match store.load_policies_for_tenant(&tenant).await {
        Ok(rows) => {
            let enabled_count = rows.iter().filter(|row| row.enabled).count();
            let disabled_count = rows.len() - enabled_count;
            let policies: Vec<serde_json::Value> = rows.iter().map(policy_row_to_json).collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "tenant": tenant,
                    "policies": policies,
                    "total": rows.len(),
                    "enabled_count": enabled_count,
                    "disabled_count": disabled_count,
                })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, tenant, "failed to list policies");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list policies: {error}"),
            )
                .into_response()
        }
    }
}

/// GET /api/policies — list policies across all tenants (admin only).
#[instrument(skip_all, fields(otel.name = "GET /api/policies"))]
pub(crate) async fn handle_list_all_policies(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) =
        crate::authz::require_observe_auth(&state, &headers, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required for cross-tenant access").into_response();
    }

    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    let discovery_rows = match store.load_all_policies().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "failed to list policies from durable store");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list policies: {error}"),
            )
                .into_response();
        }
    };
    let mut tenants = discovery_rows
        .iter()
        .map(|row| row.tenant.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if let Ok(registry) = state.registry.read() {
        tenants.extend(
            registry
                .tenant_ids()
                .into_iter()
                .map(|tenant| tenant.as_str().to_string()),
        );
    }
    let authorization_tenant = headers
        .get("x-tenant-id")
        .and_then(|value| value.to_str().ok())
        .filter(|tenant| !tenant.is_empty())
        .unwrap_or("system")
        .to_string();
    tenants.insert(authorization_tenant);
    let guarded_tenants = tenants.clone();
    let mut _generation_guards = Vec::with_capacity(tenants.len());
    for tenant in tenants {
        match begin_policy_generation_read(&state, &tenant).await {
            Ok(guard) => _generation_guards.push(guard),
            Err(response) => return response,
        }
    }
    if let Err(status) =
        crate::authz::require_observe_auth(&state, &headers, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required for cross-tenant access").into_response();
    }
    let mut rows = match store.load_all_policies().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "failed to list stable policies from durable store");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list policies: {error}"),
            )
                .into_response();
        }
    };
    if rows
        .iter()
        .any(|row| !guarded_tenants.contains(&row.tenant))
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Policy tenant set changed while acquiring stable generations; retry",
        )
            .into_response();
    }

    rows.sort_by(|a, b| {
        a.tenant
            .cmp(&b.tenant)
            .then_with(|| a.policy_id.cmp(&b.policy_id))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });

    let total = rows.len();
    let mut by_tenant = std::collections::BTreeMap::new();
    for row in &rows {
        *by_tenant.entry(row.tenant.clone()).or_insert(0usize) += 1;
    }
    let policies: Vec<serde_json::Value> = rows.iter().map(policy_row_to_json).collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "policies": policies,
            "total": total,
            "by_tenant": by_tenant,
        })),
    )
        .into_response()
}
