//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityUid, Policy, PolicyId, PolicySet,
    Request, Response as CedarResponse,
};
use opentelemetry::global;
use opentelemetry::metrics::Counter;

use crate::context::{PrincipalKind, SecurityContext};
use crate::error::{AuthzDenial, AuthzError};

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

/// Per-tenant policy data: the compiled `PolicySet` and the source text.
struct TenantPolicies {
    policy_set: PolicySet,
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
    fallback_policy_set: RwLock<PolicySet>,
    authorizer: Authorizer,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text (loaded into the
    /// fallback global policy set).
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        let policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        Ok(Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(policy_set),
            authorizer: Authorizer::new(),
        })
    }

    /// Create an AuthzEngine with no policies (Cedar default-deny semantics).
    ///
    /// Use this to test deny behavior. For test setups that need all requests
    /// to be allowed, use [`permissive`](Self::permissive) instead.
    pub fn empty() -> Self {
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(PolicySet::new()),
            authorizer: Authorizer::new(),
        }
    }

    /// Create an AuthzEngine that permits all requests.
    ///
    /// Loads a single catch-all `permit(principal, action, resource);` policy
    /// so that Cedar evaluates to Allow even for non-System principals.
    pub fn permissive() -> Self {
        let policy_set =
            PolicySet::from_str("permit(principal, action, resource);").unwrap_or_default();
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(policy_set),
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
        let new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policy_set: new_policy_set,
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

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policy_set: combined_set,
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
        let new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        let mut current = self
            .fallback_policy_set
            .write()
            .map_err(|e| AuthzError::Engine(format!("policy lock poisoned: {e}")))?;
        *current = new_policy_set;
        Ok(())
    }

    /// Returns the total number of policies across all tenants + fallback.
    pub fn policy_count(&self) -> usize {
        let tenant_count = self
            .tenant_policies
            .read()
            .map(|t| t.values().map(|tp| tp.policy_set.policies().count()).sum())
            .unwrap_or(0);
        let fallback_count = self
            .fallback_policy_set
            .read()
            .map_or(0, |ps| ps.policies().count());
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
                &tp.policy_set,
            )
        } else {
            // No per-tenant policies loaded — fall back to global.
            drop(tenants);
            self.authorize(security_ctx, action, resource_type, resource_attrs)
        }
    }

    /// Core Cedar evaluation logic shared by both `authorize` and
    /// `authorize_for_tenant`.
    fn evaluate_request(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
        policy_set: &PolicySet,
    ) -> AuthzDecision {
        cedar_evaluations_counter().add(1, &[]);

        // Build Cedar principal
        let principal_type = match security_ctx.principal.kind {
            PrincipalKind::Customer => "Customer",
            PrincipalKind::Agent => "Agent",
            PrincipalKind::Admin => "Admin",
            PrincipalKind::System => "System",
        };

        let principal_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            principal_type, security_ctx.principal.id
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::InvalidPrincipal(e.to_string()));
            }
        };

        // Build Cedar action
        let action_uid = match EntityUid::from_str(&format!("Action::\"{}\"", action)) {
            Ok(uid) => uid,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::InvalidAction(e.to_string()));
            }
        };

        // Build Cedar resource
        let resource_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            resource_type,
            resource_id_from_attrs(resource_attrs)
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::InvalidResource(e.to_string()));
            }
        };

        // Build Cedar context from security context attrs + resource attrs
        let mut ctx_map: HashMap<String, cedar_policy::RestrictedExpression> = HashMap::new();

        // Add principal attributes to context
        if let Some(ref role) = security_ctx.principal.role {
            ctx_map.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        if let Some(ref acting_for) = security_ctx.principal.acting_for {
            ctx_map.insert(
                "actingFor".to_string(),
                cedar_policy::RestrictedExpression::new_string(acting_for.clone()),
            );
        }

        // Add context attributes
        for (key, value) in &security_ctx.context_attrs {
            insert_json_as_cedar(&mut ctx_map, key.clone(), value);
        }

        // Inject resource attributes into context (enables Cedar policies to
        // reference entity state and cross-entity context via `context.key`).
        for (key, value) in resource_attrs {
            insert_json_as_cedar(&mut ctx_map, key.clone(), value);
        }

        // Build context and request
        let context = match Context::from_pairs(ctx_map) {
            Ok(c) => c,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::InvalidContext(e.to_string()));
            }
        };

        // Build principal entity with attributes so Cedar can resolve both
        // exact UID matches (`principal == Agent::"bot-1"`) and attribute
        // access (`principal.agent_type in [...]`).
        let mut principal_attrs: HashMap<String, cedar_policy::RestrictedExpression> =
            HashMap::new();
        principal_attrs.insert(
            "id".to_string(),
            cedar_policy::RestrictedExpression::new_string(security_ctx.principal.id.clone()),
        );
        if let Some(ref agent_type) = security_ctx.principal.agent_type {
            principal_attrs.insert(
                "agent_type".to_string(),
                cedar_policy::RestrictedExpression::new_string(agent_type.clone()),
            );
        }
        if let Some(ref role) = security_ctx.principal.role {
            principal_attrs.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        for (key, value) in &security_ctx.principal.attributes {
            insert_json_as_cedar(&mut principal_attrs, key.clone(), value);
        }

        // Entity schema validation is intentionally None: principal attributes
        // include tenant-defined custom attrs that can't be predicted by a
        // static schema. Policy-level type checking suffices.

        let entities = match Entity::new(principal_uid.clone(), principal_attrs, HashSet::new()) {
            Ok(entity) => match Entities::from_entities([entity], None) {
                Ok(e) => e,
                Err(e) => {
                    return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                        "failed to build entity store: {e}"
                    )));
                }
            },
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "failed to build principal entity: {e}"
                )));
            }
        };

        let request = match Request::new(
            principal_uid,
            action_uid,
            resource_uid,
            context,
            None, // schema-less: actions/resources are tenant-defined
        ) {
            Ok(r) => r,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "invalid request: {e}"
                )));
            }
        };

        let response: CedarResponse = self
            .authorizer
            .is_authorized(&request, policy_set, &entities);

        match response.decision() {
            Decision::Allow => {
                let policy_ids: Vec<String> = response
                    .diagnostics()
                    .reason()
                    .map(|id| id.to_string())
                    .collect();
                AuthzDecision::Allow { policy_ids }
            }
            Decision::Deny => {
                let policy_ids: Vec<String> = response
                    .diagnostics()
                    .reason()
                    .map(|id| id.to_string())
                    .collect();
                if policy_ids.is_empty() {
                    AuthzDecision::Deny(AuthzDenial::NoMatchingPermit)
                } else {
                    AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids })
                }
            }
        }
    }

    /// Quick check: is this a system principal (bypasses all checks)?
    pub fn is_system(security_ctx: &SecurityContext) -> bool {
        security_ctx.principal.kind == PrincipalKind::System
    }

    /// Authorize with system bypass: system principals always allowed.
    /// Uses fallback global policy set.
    pub fn authorize_or_bypass(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        if Self::is_system(security_ctx) {
            return AuthzDecision::Allow {
                policy_ids: vec!["system-bypass".to_string()],
            };
        }
        self.authorize(security_ctx, action, resource_type, resource_attrs)
    }

    /// Authorize for a specific tenant with system bypass.
    pub fn authorize_for_tenant_or_bypass(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        if Self::is_system(security_ctx) {
            return AuthzDecision::Allow {
                policy_ids: vec!["system-bypass".to_string()],
            };
        }
        self.authorize_for_tenant(tenant, security_ctx, action, resource_type, resource_attrs)
    }
}

fn cedar_evaluations_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("temper-authz")
            .u64_counter("temper_cedar_evaluations_total")
            .with_description("Total number of Cedar authorization evaluations.")
            .build()
    })
}

/// Insert a `serde_json::Value` into a Cedar context map, converting to the
/// appropriate `RestrictedExpression` type. Supports strings, bools, integers,
/// and arrays of those types.
fn insert_json_as_cedar(
    map: &mut HashMap<String, cedar_policy::RestrictedExpression>,
    key: String,
    value: &serde_json::Value,
) {
    if let Some(s) = value.as_str() {
        map.insert(
            key,
            cedar_policy::RestrictedExpression::new_string(s.to_string()),
        );
    } else if let Some(b) = value.as_bool() {
        map.insert(key, cedar_policy::RestrictedExpression::new_bool(b));
    } else if let Some(n) = value.as_i64() {
        map.insert(key, cedar_policy::RestrictedExpression::new_long(n));
    } else if let Some(arr) = value.as_array() {
        let items: Vec<cedar_policy::RestrictedExpression> = arr
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(cedar_policy::RestrictedExpression::new_string(
                        s.to_string(),
                    ))
                } else if let Some(n) = item.as_i64() {
                    Some(cedar_policy::RestrictedExpression::new_long(n))
                } else {
                    item.as_bool()
                        .map(cedar_policy::RestrictedExpression::new_bool)
                }
            })
            .collect();
        map.insert(key, cedar_policy::RestrictedExpression::new_set(items));
    }
}

fn resource_id_from_attrs(attrs: &HashMap<String, serde_json::Value>) -> String {
    attrs
        .get("id")
        .or_else(|| attrs.get("Id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}
