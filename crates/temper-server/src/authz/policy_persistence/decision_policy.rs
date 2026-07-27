//! Immutable decision-scoped policy receipt and publication ownership.

use cedar_policy::{PolicyId, PolicySet};
use serde::{Deserialize, Serialize};
use temper_authz::PolicyScopeMatrix;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::{
    PolicyPublicationError, load_and_activate_tenant_policies, publish_policy_snapshot,
    record_policy_saved,
};
use crate::state::ServerState;
use crate::storage::PolicyStoreRow;

/// Immutable approval data carried through `GovernanceDecision.Approve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionPolicyReceipt {
    /// Durable pending-decision id whose policy row is `decision:{id}`.
    pub pending_decision_id: String,
    /// GovernanceDecision actor id authorized to consume this receipt.
    pub governance_decision_id: String,
    /// Exact Cedar principal entity type observed at denial time.
    pub principal_kind: String,
    /// Complete, human-approved policy scope.
    pub scope_matrix: PolicyScopeMatrix,
}

impl DecisionPolicyReceipt {
    /// Serialize the receipt for the GovernanceDecision `scope` action field.
    pub fn encode(&self) -> Result<String, String> {
        serde_json::to_string(self)
            .map_err(|error| format!("failed to encode decision policy receipt: {error}"))
    }

    /// Parse a receipt from the GovernanceDecision `scope` state field.
    pub fn decode(encoded: &str) -> Result<Self, String> {
        serde_json::from_str(encoded)
            .map_err(|error| format!("invalid decision policy receipt: {error}"))
    }

    /// Return the one durable policy id owned by this decision.
    pub fn policy_id(&self) -> String {
        format!("decision:{}", self.pending_decision_id)
    }
}

/// Result of installing a decision-scoped policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionPolicyInstall {
    /// A new durable policy row was created and activated.
    Created { publication_version: u64 },
    /// The exact durable row already existed and was reloaded idempotently.
    AlreadyPresent { publication_version: u64 },
}

/// Install one immutable, decision-id-keyed policy through snapshot CAS.
#[instrument(skip_all, fields(tenant, policy_id, otel.name = "authz.install_decision_policy"))]
pub async fn install_decision_policy(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
) -> Result<DecisionPolicyInstall, String> {
    if policy_id
        .strip_prefix("decision:")
        .is_none_or(str::is_empty)
    {
        return Err("decision policy id must use the decision:<id> form".to_string());
    }
    let store = state
        .policy_store()
        .ok_or_else(|| "durable policy store is not configured".to_string())?;
    let created_at = sim_now().to_rfc3339();
    for _attempt in 0..4 {
        let before = store
            .load_policy_snapshot(tenant)
            .await
            .map_err(|error| format!("failed to load durable policies: {error}"))?;
        if let Some(existing) = before.rows.iter().find(|row| row.policy_id == policy_id) {
            if !existing.enabled {
                return Err(format!(
                    "decision policy {policy_id:?} exists but is disabled"
                ));
            }
            if existing.cedar_text != cedar_text {
                return Err(format!(
                    "decision policy {policy_id:?} already exists with different approved content"
                ));
            }
            let publication_version = load_and_activate_tenant_policies(state, tenant).await?;
            return Ok(DecisionPolicyInstall::AlreadyPresent {
                publication_version,
            });
        }

        let mut prospective = before.rows;
        prospective.push(PolicyStoreRow {
            tenant: tenant.to_string(),
            policy_id: policy_id.to_string(),
            cedar_text: cedar_text.to_string(),
            policy_hash: String::new(),
            created_at: created_at.clone(),
            created_by: created_by.to_string(),
            enabled: true,
        });
        match publish_policy_snapshot(state, tenant, before.version, prospective).await {
            Ok(committed) => {
                record_policy_saved(state, tenant, policy_id, created_by);
                return Ok(DecisionPolicyInstall::Created {
                    publication_version: committed.version,
                });
            }
            Err(PolicyPublicationError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("decision policy publication remained contended after retry budget".to_string())
}

/// Remove only the exact publication created by a failed approval attempt.
pub async fn rollback_created_decision_policy(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    publication_version: u64,
) -> Result<(), String> {
    let store = state
        .policy_store()
        .ok_or_else(|| "durable policy store is not configured".to_string())?;
    let current = store
        .load_policy_snapshot(tenant)
        .await
        .map_err(|error| format!("failed to load policies for rollback: {error}"))?;
    if !current.rows.iter().any(|row| row.policy_id == policy_id) {
        load_and_activate_tenant_policies(state, tenant).await?;
        return Ok(());
    }
    if current.version != publication_version {
        return Err(format!(
            "cannot roll back decision policy {policy_id:?}: publication advanced from owned version {publication_version} to {}",
            current.version
        ));
    }
    let remaining = current
        .rows
        .into_iter()
        .filter(|row| row.policy_id != policy_id)
        .collect();
    publish_policy_snapshot(state, tenant, current.version, remaining)
        .await
        .map(|_| ())
        .map_err(|error| format!("failed to roll back decision policy: {error}"))
}

/// Verify that an exact policy AST occurs once in the tenant's active set.
pub fn verify_active_policy_exactly_once(
    state: &ServerState,
    tenant: &str,
    cedar_text: &str,
) -> Result<(), String> {
    let expected_set: PolicySet = cedar_text
        .parse()
        .map_err(|error| format!("generated decision policy is invalid: {error}"))?;
    let expected = expected_set
        .policies()
        .next()
        .ok_or_else(|| "generated decision policy contains no policy".to_string())?;
    if expected_set.policies().count() != 1 {
        return Err("generated decision policy must contain exactly one policy".to_string());
    }
    let normalized_id = PolicyId::new("decision-receipt-verification");
    let expected = expected.new_id(normalized_id.clone());
    let active_text = state
        .authz
        .get_tenant_policy_text(tenant)
        .ok_or_else(|| format!("tenant {tenant:?} has no active policy set"))?;
    let active_set: PolicySet = active_text
        .parse()
        .map_err(|error| format!("active tenant policy set is invalid: {error}"))?;
    let matches = active_set
        .policies()
        .filter(|policy| policy.new_id(normalized_id.clone()) == expected)
        .count();
    match matches {
        1 => Ok(()),
        0 => Err("approved decision policy is not active".to_string()),
        count => Err(format!(
            "approved decision policy is active {count} times; expected exactly once"
        )),
    }
}
