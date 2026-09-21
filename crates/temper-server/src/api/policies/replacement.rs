//! Conditional durable replacement; never activate an uncommitted proposal.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use temper_authz::AuthzEngine;

use super::super::PolicyAuthed;
use crate::{state::ServerState, storage::PolicyStore};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Replacement {
    expected_hash: String,
    cedar_text: String,
}

/// POST one enabled entry, conditional on the version reviewed by its caller.
pub(crate) async fn handle_replace_policy(
    State(state): State<ServerState>,
    Path((_tenant, policy_id)): Path<(String, String)>,
    auth: PolicyAuthed,
    Json(proposal): Json<Replacement>,
) -> Response {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str();
    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Durable policy store required",
        )
            .into_response();
    };
    match replace_and_activate(
        &state.authz,
        store.as_ref(),
        tenant,
        &policy_id,
        &proposal,
        &auth.security_context().principal.id,
    )
    .await
    {
        Ok(text) => {
            state
                .tenant_policies
                .write()
                .expect("tenant policy cache lock poisoned")
                .insert(tenant.to_owned(), text);
            crate::authz::record_policy_change(
                &state,
                tenant,
                &policy_id,
                &auth.security_context().principal.id,
            );
            Json(
                json!({"status":"verified", "tenant":tenant, "policy_id":policy_id,
                "policy_hash":hash(&proposal.cedar_text), "enabled":true}),
            )
            .into_response()
        }
        Err((status, message)) => (status, Json(json!({"error":message}))).into_response(),
    }
}

type ReplacementResult = Result<String, (StatusCode, &'static str)>;

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

async fn replace_and_activate(
    authz: &AuthzEngine,
    store: &dyn PolicyStore,
    tenant: &str,
    policy_id: &str,
    proposal: &Replacement,
    created_by: &str,
) -> ReplacementResult {
    if proposal.cedar_text.is_empty()
        || proposal.cedar_text.len() > 2 * 1024 * 1024
        || proposal.expected_hash.len() != 64
        || !proposal
            .expected_hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Expected lowercase SHA-256 and 1..2097152 bytes of Cedar",
        ));
    }
    // Parse without mutating the active engine.
    let parsed = proposal
        .cedar_text
        .parse::<cedar_policy::PolicySet>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid Cedar proposal"))?;
    if parsed.policies().next().is_none() || parsed.templates().next().is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Replacement requires concrete Cedar policies",
        ));
    }
    validate_prospective(store, tenant, policy_id, proposal).await?;
    let changed = store
        .replace_policy_if_hash(
            tenant,
            policy_id,
            &proposal.expected_hash,
            &proposal.cedar_text,
            created_by,
        )
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Policy write failed; read current state before retrying",
            )
        })?;
    if !changed {
        return Err((
            StatusCode::CONFLICT,
            "Entry missing, disabled or changed; obtain fresh human approval",
        ));
    }
    let rows = store.load_policies_for_tenant(tenant).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Policy committed; durable readback failed, activation unverified",
        )
    })?;
    if !rows.iter().any(|row| {
        row.policy_id == policy_id
            && row.enabled
            && row.cedar_text == proposal.cedar_text
            && row.policy_hash == hash(&proposal.cedar_text)
    }) {
        return Err((
            StatusCode::CONFLICT,
            "Policy committed but changed again; activation unverified",
        ));
    }
    let named: Vec<_> = rows
        .iter()
        .filter(|row| row.enabled)
        .map(|row| (row.policy_id.clone(), row.cedar_text.clone()))
        .collect();
    authz
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Policy committed; active reload failed",
            )
        })?;
    let expected = named
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if authz.get_tenant_policy_text(tenant).as_deref() != Some(expected.as_str()) {
        return Err((
            StatusCode::CONFLICT,
            "Policy committed; active policy changed during verification",
        ));
    }
    Ok(expected)
}

// Validate the named combined set in a temporary engine, never the serving engine.
async fn validate_prospective(
    store: &dyn PolicyStore,
    tenant: &str,
    policy_id: &str,
    proposal: &Replacement,
) -> Result<(), (StatusCode, &'static str)> {
    let mut rows = store.load_policies_for_tenant(tenant).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Current policy read failed; no write attempted",
        )
    })?;
    let current = rows
        .iter_mut()
        .find(|row| {
            row.policy_id == policy_id
                && row.enabled
                && row.policy_hash == proposal.expected_hash
                && hash(&row.cedar_text) == proposal.expected_hash
        })
        .ok_or((
            StatusCode::CONFLICT,
            "Entry missing, disabled or changed; obtain fresh human approval",
        ))?;
    current.cedar_text.clone_from(&proposal.cedar_text);
    let named = rows
        .into_iter()
        .filter(|row| row.enabled)
        .map(|row| (row.policy_id, row.cedar_text))
        .collect::<Vec<_>>();
    AuthzEngine::empty()
        .reload_tenant_policies_named(tenant, &named)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Proposal conflicts with the current named Cedar policy set",
            )
        })
}

#[cfg(test)]
#[path = "replacement_test.rs"]
mod tests;
