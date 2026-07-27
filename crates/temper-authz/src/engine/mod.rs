//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::RwLock;

use cedar_policy::{Authorizer, Policy, PolicyId, PolicySet};

use crate::context::{PrincipalKind, SecurityContext};
use crate::error::{AuthzDenial, AuthzError};

mod candidates;
mod evaluate;

#[cfg(test)]
mod tests;

/// The result of an authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// The request is allowed, with the policy IDs that contributed to the permit.
    Allow { policy_ids: Vec<String> },
    /// The request is denied with typed denial details.
    Deny(AuthzDenial),
}

impl AuthzDecision {
    /// Returns `true` if the authorization decision is `Allow`.
    pub fn is_allowed(&self) -> bool {
        matches!(self, AuthzDecision::Allow { .. })
    }

    /// Returns the denial details if the decision is `Deny`.
    pub fn denial(&self) -> Option<&AuthzDenial> {
        match self {
            AuthzDecision::Allow { .. } => None,
            AuthzDecision::Deny(d) => Some(d),
        }
    }

    /// Returns the policy IDs that contributed to the allow decision.
    pub fn policy_ids(&self) -> &[String] {
        match self {
            AuthzDecision::Allow { policy_ids } => policy_ids,
            AuthzDecision::Deny(_) => &[],
        }
    }
}

/// Compiled policy data used by Cedar and the request-time candidate selector.
struct CompiledPolicies {
    policy_set: PolicySet,
    candidate_index: candidates::CandidatePolicyIndex,
}

impl CompiledPolicies {
    fn new(policy_set: PolicySet) -> Self {
        let candidate_index = candidates::CandidatePolicyIndex::new(&policy_set);
        Self {
            policy_set,
            candidate_index,
        }
    }
}

/// Per-tenant policy data: the compiled policies and the source text.
struct TenantPolicies {
    policies: CompiledPolicies,
    source_text: String,
}

/// The authorization engine. Holds per-tenant compiled Cedar policies and
/// evaluates authorization requests. Supports hot-reload of policies via
/// [`reload_tenant_policies`](AuthzEngine::reload_tenant_policies).
///
/// Uses `BTreeMap` for deterministic iteration order (DST compliance).
pub struct AuthzEngine {
    /// Per-tenant policy sets. Each tenant has its own isolated PolicySet.
    tenant_policies: RwLock<BTreeMap<String, TenantPolicies>>,
    /// Fallback global policy set for callers that don't specify a tenant.
    /// Deprecated: callers should migrate to `authorize_for_tenant`.
    fallback_policy_set: RwLock<CompiledPolicies>,
    authorizer: Authorizer,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text (loaded into the
    /// fallback global policy set). ADR-0046: the built-in `system-platform`
    /// policy is merged in so System principals are authorized by an
    /// explicit, auditable policy rather than a hard-coded bypass.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        // Parse the user policy first — return an error if it's malformed.
        let mut policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut policy_set);

        Ok(Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        })
    }

    /// Create an AuthzEngine with no user policies, but with the built-in
    /// `system-platform` policy installed (ADR-0046). System principals
    /// remain authorized; everything else hits Cedar's default-deny.
    ///
    /// Use this to test deny behavior for non-System principals. For test
    /// setups that need all requests to be allowed, use
    /// [`permissive`](Self::permissive) instead.
    pub fn empty() -> Self {
        let mut policy_set = PolicySet::new();
        merge_system_platform_policy(&mut policy_set);
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        }
    }

    /// Create an AuthzEngine that permits all requests.
    ///
    /// Loads a single catch-all `permit(principal, action, resource);` policy
    /// so that Cedar evaluates to Allow for every principal kind (System or
    /// otherwise). Used in tests and permissive dev environments.
    pub fn permissive() -> Self {
        let policy_set =
            PolicySet::from_str("permit(principal, action, resource);").unwrap_or_default();
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        }
    }

    /// Hot-reload Cedar policies for a specific tenant. Parses and validates
    /// the new policy text, then atomically swaps the tenant's policy set.
    /// If parsing fails, existing policies remain in effect.
    pub fn reload_tenant_policies(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policies: CompiledPolicies::new(new_policy_set),
                source_text: policy_text.to_string(),
            },
        );
        Ok(())
    }

    /// Hot-reload Cedar policies for a tenant from individually named policy
    /// entries. Each `(policy_id, cedar_text)` pair is parsed individually and
    /// assigned a meaningful `PolicyId` of the form `"{tenant}:{policy_id}"`.
    ///
    /// Multiple permit/forbid statements in one `cedar_text` are suffixed:
    /// `"{tenant}:{policy_id}:0"`, `":1"`, etc.
    ///
    /// This enables meaningful policy IDs in denial diagnostics instead of
    /// auto-generated names like `"policy0"`.
    pub fn reload_tenant_policies_named(
        &self,
        tenant: &str,
        policies: &[(String, String)], // (policy_id, cedar_text)
    ) -> Result<(), AuthzError> {
        let mut combined_set = PolicySet::new();
        let mut combined_text = String::new();

        for (policy_id, cedar_text) in policies {
            // Parse each entry's Cedar text individually.
            let entry_set: PolicySet = cedar_text
                .parse()
                .map_err(|e| AuthzError::PolicyParse(format!("{policy_id}: {e}")))?;

            // Re-add each policy with a meaningful PolicyId.
            let entry_policies: Vec<Policy> = entry_set.policies().cloned().collect();
            if entry_policies.len() == 1 {
                let named = entry_policies
                    .into_iter()
                    .next()
                    .unwrap() // ci-ok: checked len == 1
                    .new_id(PolicyId::new(format!("{tenant}:{policy_id}")));
                combined_set
                    .add(named)
                    .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
            } else {
                for (idx, p) in entry_policies.into_iter().enumerate() {
                    let named = p.new_id(PolicyId::new(format!("{tenant}:{policy_id}:{idx}")));
                    combined_set
                        .add(named)
                        .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
                }
            }

            if !combined_text.is_empty() {
                combined_text.push('\n');
            }
            combined_text.push_str(cedar_text);
        }

        merge_system_platform_policy(&mut combined_set);

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policies: CompiledPolicies::new(combined_set),
                source_text: combined_text,
            },
        );
        Ok(())
    }

    /// Remove a tenant's policy set entirely.
    pub fn remove_tenant(&self, tenant: &str) {
        if let Ok(mut tenants) = self.tenant_policies.write() {
            tenants.remove(tenant);
        }
    }

    /// Get the combined Cedar policy text for a tenant.
    pub fn get_tenant_policy_text(&self, tenant: &str) -> Option<String> {
        self.tenant_policies
            .read()
            .ok()
            .and_then(|t| t.get(tenant).map(|tp| tp.source_text.clone()))
    }

    /// Hot-reload Cedar policies into the fallback global policy set.
    ///
    /// **Deprecated**: Use [`reload_tenant_policies`](Self::reload_tenant_policies)
    /// for per-tenant isolation. This method exists for backward compatibility
    /// during migration.
    pub fn reload_policies(&self, policy_text: &str) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);

        let mut current = self
            .fallback_policy_set
            .write()
            .map_err(|e| AuthzError::Engine(format!("policy lock poisoned: {e}")))?;
        *current = CompiledPolicies::new(new_policy_set);
        Ok(())
    }

    /// Returns the total number of policies across all tenants + fallback.
    pub fn policy_count(&self) -> usize {
        let tenant_count: usize = self
            .tenant_policies
            .read()
            .map(|t| {
                t.values()
                    .map(|tp| count_user_policies(&tp.policies.policy_set))
                    .sum()
            })
            .unwrap_or(0);
        let fallback_count = self
            .fallback_policy_set
            .read()
            .map_or(0, |ps| count_user_policies(&ps.policy_set));
        tenant_count + fallback_count
    }

    /// Evaluate an authorization request against the fallback global policy set.
    ///
    /// **Prefer [`authorize_for_tenant`](Self::authorize_for_tenant)** for
    /// per-tenant isolation. This method exists for backward compatibility.
    pub fn authorize(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        let policy_set = match self.fallback_policy_set.read() {
            Ok(ps) => ps,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "policy lock poisoned: {e}"
                )));
            }
        };
        self.evaluate_request(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            &policy_set,
        )
    }

    /// Evaluate an authorization request against a specific tenant's policy set.
    ///
    /// If the tenant has no policies loaded, falls back to Cedar default-deny
    /// (returns `NoMatchingPermit`).
    pub fn authorize_for_tenant(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        let tenants = match self.tenant_policies.read() {
            Ok(t) => t,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "tenant policy lock poisoned: {e}"
                )));
            }
        };

        if let Some(tp) = tenants.get(tenant) {
            self.evaluate_request(
                security_ctx,
                action,
                resource_type,
                resource_attrs,
                &tp.policies,
            )
        } else {
            // No per-tenant policies loaded — fall back to global.
            drop(tenants);
            self.authorize(security_ctx, action, resource_type, resource_attrs)
        }
    }

    ///
    /// Since ADR-0046, this no longer bypasses authorization. It is kept as a
    /// convenience predicate for callers that want to branch on principal
    /// kind for reasons other than authorization (logging, telemetry tagging).
    /// Actual authorization of System principals flows through the normal
    /// Cedar evaluation, matching the built-in `system-platform` policy
    /// installed at engine construction time (see `SYSTEM_PLATFORM_POLICY`).
    pub fn is_system(security_ctx: &SecurityContext) -> bool {
        security_ctx.principal.kind == PrincipalKind::System
    }

    /// Authorize through the fallback global policy set.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform` Cedar
    /// policy; delegating straight to [`authorize`] ensures every request is
    /// policy-checked and logged.
    pub fn authorize_or_bypass(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize(security_ctx, action, resource_type, resource_attrs)
    }

    /// Authorize for a specific tenant through Cedar.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform`
    /// policy merged into the fallback policy set; this function simply
    /// delegates to [`authorize_for_tenant`].
    pub fn authorize_for_tenant_or_bypass(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize_for_tenant(tenant, security_ctx, action, resource_type, resource_attrs)
    }
}

/// Built-in Cedar policy granting System-kind principals broad authority
/// (ADR-0046). Installed into every [`AuthzEngine`] at construction time so
/// that platform code paths using `AgentContext::system()` continue to
/// function after the blanket bypass was removed.
///
/// This is intentionally broad for day-one migration — it preserves the
/// pre-ADR-0046 behavior of System principals being universally allowed,
/// but makes that authority an auditable, overridable Cedar policy rather
/// than hard-coded control flow. Follow-up work narrows this policy to the
/// specific actions the platform genuinely needs (bootstrap writes,
/// credential rotation, recovery).
const SYSTEM_PLATFORM_POLICY: &str = r#"
@id("system-platform:broad-permit")
permit(principal is System, action, resource);
"#;

/// PolicyId prefix used for the built-in system-platform policies
/// (ADR-0046). Used to exclude them from user-facing counts.
const SYSTEM_PLATFORM_POLICY_ID_PREFIX: &str = "system-platform:";

/// Merge the built-in system-platform policy into an existing [`PolicySet`].
///
/// Policies are added with explicit `PolicyId`s prefixed by
/// [`SYSTEM_PLATFORM_POLICY_ID_PREFIX`] so downstream code can filter them
/// out of user-facing reports (see [`count_user_policies`]). If the
/// hard-coded system policy fails to parse, the combined set is left
/// unchanged — preserving availability at the cost of System auth.
fn merge_system_platform_policy(combined: &mut PolicySet) {
    let system_set: PolicySet = match SYSTEM_PLATFORM_POLICY.parse() {
        Ok(ps) => ps,
        Err(_) => return,
    };
    for (idx, policy) in system_set.policies().enumerate() {
        let named = policy.clone().new_id(PolicyId::new(format!(
            "{SYSTEM_PLATFORM_POLICY_ID_PREFIX}broad-permit-{idx}"
        )));
        let _ = combined.add(named);
    }
}

/// Count user-authored policies in a [`PolicySet`], excluding the built-in
/// `system-platform` policies (ADR-0046). Tenants should reason about their
/// own policy surface without the platform's internals polluting the count.
fn count_user_policies(ps: &PolicySet) -> usize {
    ps.policies()
        .filter(|p| {
            !p.id()
                .to_string()
                .starts_with(SYSTEM_PLATFORM_POLICY_ID_PREFIX)
        })
        .count()
}
