//! Durable one-owner coordination shared by approve and deny.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::state::{
    DecisionResolutionKind, DecisionResolutionPhase, DecisionStatus, PendingDecision,
};
use crate::storage::MetadataStore;

fn write_component(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

pub(super) fn resolution_owner(
    decision: &PendingDecision,
    kind: DecisionResolutionKind,
    request_binding: &str,
) -> String {
    let mut hasher = Sha256::new();
    write_component(&mut hasher, &decision.tenant);
    write_component(&mut hasher, &decision.id);
    write_component(
        &mut hasher,
        match kind {
            DecisionResolutionKind::Approve => "approve",
            DecisionResolutionKind::Deny => "deny",
        },
    );
    write_component(&mut hasher, request_binding);
    format!("resolution:{:x}", hasher.finalize())
}

async fn reload_decision(
    store: &Arc<dyn MetadataStore>,
    tenant: &str,
    id: &str,
) -> Result<PendingDecision, String> {
    let data = store
        .get_pending_decision(id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "decision disappeared during resolution".to_string())?;
    let decision: PendingDecision =
        serde_json::from_str(&data).map_err(|error| error.to_string())?;
    if decision.tenant != tenant {
        return Err("decision tenant changed during resolution".to_string());
    }
    Ok(decision)
}

pub(super) async fn claim_or_resume(
    store: &Arc<dyn MetadataStore>,
    decision: &PendingDecision,
    owner: &str,
    kind: DecisionResolutionKind,
) -> Result<PendingDecision, String> {
    if decision.status != DecisionStatus::Pending {
        return Err(format!(
            "decision already resolved as {:?}",
            decision.status
        ));
    }
    if let Some(existing_owner) = decision.resolution_owner.as_deref() {
        if existing_owner == owner && decision.resolution_kind == Some(kind) {
            return Ok(decision.clone());
        }
        return Err("decision is already owned by a different resolution".to_string());
    }
    let mut claimed = decision.clone();
    claimed.resolution_owner = Some(owner.to_string());
    claimed.resolution_kind = Some(kind);
    claimed.resolution_phase = Some(DecisionResolutionPhase::Claimed);
    let claimed_json = serde_json::to_string(&claimed).map_err(|error| error.to_string())?;
    if store
        .claim_decision_resolution(&decision.tenant, &decision.id, &claimed_json)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(claimed);
    }
    let raced = reload_decision(store, &decision.tenant, &decision.id).await?;
    if raced.status == DecisionStatus::Pending
        && raced.resolution_owner.as_deref() == Some(owner)
        && raced.resolution_kind == Some(kind)
    {
        return Ok(raced);
    }
    Err("decision was claimed or resolved by a competing request".to_string())
}

pub(super) async fn persist_resolution_progress(
    store: &Arc<dyn MetadataStore>,
    decision: &PendingDecision,
    owner: &str,
) -> Result<(), String> {
    let data = serde_json::to_string(decision).map_err(|error| error.to_string())?;
    let updated = store
        .update_decision_resolution(&decision.tenant, &decision.id, owner, "resolving", &data)
        .await
        .map_err(|error| error.to_string())?;
    if !updated {
        return Err("decision resolution ownership was lost".to_string());
    }
    Ok(())
}

pub(super) async fn complete_resolution(
    store: &Arc<dyn MetadataStore>,
    decision: &PendingDecision,
    owner: &str,
) -> Result<(), String> {
    let status = match decision.status {
        DecisionStatus::Approved => "approved",
        DecisionStatus::Denied => "denied",
        _ => return Err("resolution completion requires a terminal decision".to_string()),
    };
    let data = serde_json::to_string(decision).map_err(|error| error.to_string())?;
    let updated = store
        .update_decision_resolution(&decision.tenant, &decision.id, owner, status, &data)
        .await
        .map_err(|error| error.to_string())?;
    if !updated {
        return Err("decision resolution completion lost ownership".to_string());
    }
    Ok(())
}

pub(super) async fn release_resolution(
    store: &Arc<dyn MetadataStore>,
    pending: &PendingDecision,
    owner: &str,
) -> Result<(), String> {
    let mut released = pending.clone();
    released.resolution_owner = None;
    released.resolution_kind = None;
    released.resolution_phase = None;
    released.resolution_policy_version = None;
    let data = serde_json::to_string(&released).map_err(|error| error.to_string())?;
    let updated = store
        .release_decision_resolution(&pending.tenant, &pending.id, owner, &data)
        .await
        .map_err(|error| error.to_string())?;
    if !updated {
        return Err("decision resolution release lost ownership".to_string());
    }
    Ok(())
}
