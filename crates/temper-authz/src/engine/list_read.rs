//! A list read decided once for what its rows share, and per row for the rest.
//!
//! A row check used to evaluate every policy that can match the principal,
//! action and type, for each row. Most of those policies never read the row,
//! so their answer is the same for the whole list. They are evaluated once
//! here, and each row evaluates only the policies it can change
//! ([`list_uniformity::split`](super::list_uniformity::split) says which and
//! why the two results combine exactly).

use std::collections::{BTreeSet, HashMap};

use cedar_policy::{Effect, Policy, PolicySet};

use super::{AuthzDecision, AuthzEngine, CompiledPolicies, ListReadUniformity, list_uniformity};
use crate::context::SecurityContext;
use crate::error::AuthzDenial;
use crate::metrics::CedarCheck;

/// How a list read decides its rows.
pub enum ListRead {
    /// Every row may be read.
    AllowAll,
    /// No row may be read.
    DenyAll,
    /// Each row is decided by [`AuthzEngine::list_row_allowed`].
    PerRow(Box<RowRule>),
}

/// The part of a list read's decision that depends on the row.
pub struct RowRule {
    /// Resource attributes the row's policies read, or `None` when a policy
    /// reads the row in a shape that needs the whole row.
    pub attribute_keys: Option<BTreeSet<String>>,
    /// Policies evaluated for each row; 0 when each row evaluates the whole set.
    pub row_policy_count: usize,
    /// A permit that does not read the row is satisfied, so a row is read
    /// unless one of its own forbids is.
    list_permits: bool,
    /// The policies a row can change; `None` evaluates the whole set per row.
    row_policies: Option<CompiledPolicies>,
}

impl AuthzEngine {
    /// Decide what a list read's rows share. `probe_attrs` stands for a row
    /// in the policies that do not read one.
    pub fn list_read_for_tenant(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        probe_attrs: &HashMap<String, serde_json::Value>,
    ) -> ListRead {
        let Ok(tenants) = self.tenant_policies.read() else {
            return ListRead::PerRow(Box::new(whole_set_per_row()));
        };
        let policies = match tenants.get(tenant) {
            Some(tenant_policies) => &tenant_policies.policies,
            None if Self::is_system(security_ctx) => &self.platform_policy_set,
            // No tenant policy and not a system principal: Cedar default-deny,
            // whichever row it is.
            None => return ListRead::DenyAll,
        };
        let Some(split) =
            list_uniformity::split(&policies.policy_set, security_ctx, action, resource_type)
        else {
            return ListRead::PerRow(Box::new(whole_set_per_row()));
        };
        let attribute_keys = match split.uniformity {
            ListReadUniformity::Uniform => None,
            ListReadUniformity::PerEntity { attribute_keys } => attribute_keys,
        };

        let list_permits = if split.list.is_empty() {
            false
        } else {
            let Some(list_policies) = compile(split.list) else {
                return ListRead::PerRow(Box::new(whole_set_per_row()));
            };
            match self.evaluate_request(
                security_ctx,
                action,
                resource_type,
                probe_attrs,
                &list_policies,
                CedarCheck::RowFilter,
            ) {
                AuthzDecision::Allow { .. } => true,
                AuthzDecision::Deny(AuthzDenial::NoMatchingPermit) => false,
                // A forbid every row shares, or a request Cedar cannot
                // evaluate for any row.
                AuthzDecision::Deny(_) => return ListRead::DenyAll,
            }
        };

        if split.row.is_empty() {
            return if list_permits {
                ListRead::AllowAll
            } else {
                ListRead::DenyAll
            };
        }
        // Only a satisfied permit allows a row. With none from the list and
        // none among the row's policies, no row can be allowed.
        if !list_permits
            && split
                .row
                .iter()
                .all(|policy| policy.effect() == Effect::Forbid)
        {
            return ListRead::DenyAll;
        }
        let row_policy_count = split.row.len();
        let Some(row_policies) = compile(split.row) else {
            return ListRead::PerRow(Box::new(whole_set_per_row()));
        };
        ListRead::PerRow(Box::new(RowRule {
            attribute_keys,
            row_policy_count,
            list_permits,
            row_policies: Some(row_policies),
        }))
    }

    /// Whether one row of a list read may be read, given its
    /// [`RowRule`] from [`Self::list_read_for_tenant`].
    pub fn list_row_allowed(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        rule: &RowRule,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> bool {
        let Some(row_policies) = rule.row_policies.as_ref() else {
            return self
                .authorize_row_for_tenant(
                    tenant,
                    security_ctx,
                    action,
                    resource_type,
                    resource_attrs,
                )
                .is_allowed();
        };
        match self.evaluate_request(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            row_policies,
            CedarCheck::RowFilter,
        ) {
            AuthzDecision::Allow { .. } => true,
            AuthzDecision::Deny(AuthzDenial::NoMatchingPermit) => rule.list_permits,
            AuthzDecision::Deny(_) => false,
        }
    }
}

fn whole_set_per_row() -> RowRule {
    RowRule {
        attribute_keys: None,
        row_policy_count: 0,
        list_permits: false,
        row_policies: None,
    }
}

fn compile(policies: Vec<Policy>) -> Option<CompiledPolicies> {
    PolicySet::from_policies(policies)
        .ok()
        .map(CompiledPolicies::new)
}

#[cfg(test)]
#[path = "list_read_test.rs"]
mod tests;
