//! Pending authorization decision queue.
//!
//! **Deprecated**: Authorization decisions are now managed as GovernanceDecision
//! IOA entities in the `temper-system` tenant (ADR-0025). This module is
//! retained for backward compatibility during migration. New code should
//! dispatch `GovernanceDecision.CreateGovernanceDecision` via entity dispatch
//! and use `GovernanceDecision.Approve` / `.Deny` for resolution.
//!
//! When a Cedar authorization denial occurs (either at the OData layer or the
//! WASM authz gate), a `PendingDecision` is created and pushed to the bounded
//! log. The human can then approve or deny the decision via the dashboard.

use std::collections::{BTreeMap, VecDeque};
use temper_runtime::scheduler::{sim_now, sim_uuid};

/// Maximum number of pending decisions retained in the bounded log.
const PENDING_DECISION_CAPACITY: usize = 1_000;

/// Status of a pending authorization decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    /// Awaiting human review.
    Pending,
    /// Approved — Cedar policy generated and loaded.
    Approved,
    /// Denied by human.
    Denied,
    /// Expired without action.
    Expired,
}

pub use temper_authz::{
    ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope,
};

/// A pending authorization decision.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingDecision {
    /// Unique decision ID ("PD-{uuid}").
    pub id: String,
    /// Tenant where the denial occurred.
    pub tenant: String,
    /// Agent that was denied.
    pub agent_id: String,
    /// Action that was denied (e.g. "submitOrder", "http_call", "access_secret").
    pub action: String,
    /// Resource type (e.g. "Order", "HttpEndpoint", "Secret").
    pub resource_type: String,
    /// Resource ID (e.g. "order-123", "api.stripe.com", "stripe_key").
    pub resource_id: String,
    /// Additional resource attributes at the time of denial.
    pub resource_attrs: serde_json::Value,
    /// Cedar denial reason.
    pub denial_reason: String,
    /// WASM module name (if this was a WASM integration denial).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_name: Option<String>,
    /// ISO-8601 timestamp when the denial occurred.
    pub created_at: String,
    /// Current status.
    pub status: DecisionStatus,
    /// Who approved/denied (if resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// When approved/denied (if resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// Generated Cedar policy text (if approved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_policy: Option<String>,
    /// Scope of approval (if approved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_scope: Option<PolicyScopeMatrix>,
    /// Link to the evolution A-Record that analyzed this denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolution_record_id: Option<String>,
    /// Agent type that was denied (for pattern analysis and Cedar context matching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Session ID at time of denial (for session-scoped approvals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl PendingDecision {
    /// Create a new pending decision from a denial.
    #[allow(clippy::too_many_arguments)]
    pub fn from_denial(
        tenant: &str,
        agent_id: &str,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        resource_attrs: serde_json::Value,
        denial_reason: &str,
        module_name: Option<String>,
    ) -> Self {
        Self {
            id: format!("PD-{}", sim_uuid()),
            tenant: tenant.to_string(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
            resource_attrs,
            denial_reason: denial_reason.to_string(),
            module_name,
            created_at: sim_now().to_rfc3339(),
            status: DecisionStatus::Pending,
            decided_by: None,
            decided_at: None,
            generated_policy: None,
            approved_scope: None,
            evolution_record_id: None,
            agent_type: None,
            session_id: None,
        }
    }

    /// Deduplication key: same tenant + agent + action + resource_type + resource_id.
    pub fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.tenant, self.agent_id, self.action, self.resource_type, self.resource_id
        )
    }

    /// Generate Cedar policy text from a scope matrix.
    pub fn generate_policy_from_matrix(&self, matrix: &PolicyScopeMatrix) -> String {
        temper_authz::generate_cedar_from_matrix(
            &self.agent_id,
            &self.action,
            &self.resource_type,
            &self.resource_id,
            matrix,
        )
    }
}

/// Bounded, deduplicating pending decision log.
pub struct PendingDecisionLog {
    /// The bounded deque of decisions.
    entries: VecDeque<PendingDecision>,
    /// Dedup index: dedup_key -> entry ID in deque.
    dedup_keys: BTreeMap<String, String>,
}

impl Default for PendingDecisionLog {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingDecisionLog {
    /// Create a new empty log.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(PENDING_DECISION_CAPACITY),
            dedup_keys: BTreeMap::new(),
        }
    }

    /// Push a decision, deduplicating by key. Returns true if actually added (not a dup).
    pub fn push(&mut self, decision: PendingDecision) -> bool {
        let key = decision.dedup_key();
        // Check for existing pending decision with same key
        if let Some(existing_id) = self.dedup_keys.get(&key) {
            // Check if the existing decision is still pending
            if self
                .entries
                .iter()
                .any(|d| d.id == *existing_id && d.status == DecisionStatus::Pending)
            {
                return false;
            }
        }

        // Evict oldest if at capacity
        if self.entries.len() >= PENDING_DECISION_CAPACITY
            && let Some(evicted) = self.entries.pop_front()
        {
            self.dedup_keys.remove(&evicted.dedup_key());
        }

        self.dedup_keys.insert(key, decision.id.clone());
        self.entries.push_back(decision);
        true
    }

    /// Read-only access to all entries.
    pub fn entries(&self) -> &VecDeque<PendingDecision> {
        &self.entries
    }

    /// Find a decision by ID.
    pub fn get(&self, id: &str) -> Option<&PendingDecision> {
        self.entries.iter().find(|d| d.id == id)
    }

    /// Find a mutable decision by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut PendingDecision> {
        self.entries.iter_mut().find(|d| d.id == id)
    }

    /// Count decisions by status.
    pub fn count_by_status(&self, status: &DecisionStatus) -> usize {
        self.entries.iter().filter(|d| d.status == *status).count()
    }
}
