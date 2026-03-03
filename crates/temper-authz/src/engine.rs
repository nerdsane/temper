//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::RwLock;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request,
    Response as CedarResponse,
};

use crate::context::{PrincipalKind, SecurityContext};
use crate::error::AuthzError;

/// The result of an authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// The request is allowed.
    Allow,
    /// The request is denied with a reason.
    Deny(String),
}

impl AuthzDecision {
    /// Returns `true` if the authorization decision is `Allow`.
    pub fn is_allowed(&self) -> bool {
        matches!(self, AuthzDecision::Allow)
    }
}

/// The authorization engine. Holds compiled Cedar policies and evaluates
/// authorization requests. Supports hot-reload of policies via [`reload_policies`].
pub struct AuthzEngine {
    policy_set: RwLock<PolicySet>,
    authorizer: Authorizer,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        let policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        Ok(Self {
            policy_set: RwLock::new(policy_set),
            authorizer: Authorizer::new(),
        })
    }

    /// Create an AuthzEngine with no policies (denies by default per Cedar semantics).
    pub fn permissive() -> Self {
        Self {
            policy_set: RwLock::new(PolicySet::new()),
            authorizer: Authorizer::new(),
        }
    }

    /// Hot-reload Cedar policies. Parses and validates the new policy text,
    /// then atomically swaps the policy set. If parsing fails, the existing
    /// policies remain in effect and an error is returned.
    pub fn reload_policies(&self, policy_text: &str) -> Result<(), AuthzError> {
        let new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        let mut current = self
            .policy_set
            .write()
            .map_err(|e| AuthzError::Engine(format!("policy lock poisoned: {e}")))?;
        *current = new_policy_set;
        Ok(())
    }

    /// Returns the number of policies currently loaded.
    pub fn policy_count(&self) -> usize {
        self.policy_set.read().map_or(0, |ps| ps.policies().count())
    }

    /// Evaluate an authorization request.
    ///
    /// - `security_ctx`: The security context from the HTTP request
    /// - `action`: The OData action (e.g., "read", "create", "submitOrder", "cancelOrder")
    /// - `resource_type`: The entity type (e.g., "Order")
    /// - `resource_attrs`: Attributes of the resource being accessed
    pub fn authorize(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
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
                return AuthzDecision::Deny(format!("invalid principal: {e}"));
            }
        };

        // Build Cedar action
        let action_uid = match EntityUid::from_str(&format!("Action::\"{}\"", action)) {
            Ok(uid) => uid,
            Err(e) => {
                return AuthzDecision::Deny(format!("invalid action: {e}"));
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
                return AuthzDecision::Deny(format!("invalid resource: {e}"));
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
                return AuthzDecision::Deny(format!("invalid context: {e}"));
            }
        };

        let request = match Request::new(
            principal_uid,
            action_uid,
            resource_uid,
            context,
            None, // no schema validation for now
        ) {
            Ok(r) => r,
            Err(e) => {
                return AuthzDecision::Deny(format!("invalid request: {e}"));
            }
        };

        let entities = Entities::empty();

        let policy_set = match self.policy_set.read() {
            Ok(ps) => ps,
            Err(e) => return AuthzDecision::Deny(format!("policy lock poisoned: {e}")),
        };
        let response: CedarResponse =
            self.authorizer
                .is_authorized(&request, &policy_set, &entities);

        match response.decision() {
            Decision::Allow => AuthzDecision::Allow,
            Decision::Deny => {
                let reasons: Vec<String> = response
                    .diagnostics()
                    .reason()
                    .map(|id| id.to_string())
                    .collect();
                let msg = if reasons.is_empty() {
                    "no matching permit policy".to_string()
                } else {
                    reasons.join(", ")
                };
                AuthzDecision::Deny(msg)
            }
        }
    }

    /// Quick check: is this a system principal (bypasses all checks)?
    pub fn is_system(security_ctx: &SecurityContext) -> bool {
        security_ctx.principal.kind == PrincipalKind::System
    }

    /// Authorize with system bypass: system principals always allowed.
    pub fn authorize_or_bypass(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        if Self::is_system(security_ctx) {
            return AuthzDecision::Allow;
        }
        self.authorize(security_ctx, action, resource_type, resource_attrs)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::SecurityContext;

    fn admin_context() -> SecurityContext {
        SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), "admin-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
        ])
    }

    fn customer_context(id: &str) -> SecurityContext {
        SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), id.to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ])
    }

    #[test]
    fn test_permissive_engine_denies_by_default() {
        // No policies = no permits = deny
        let engine = AuthzEngine::permissive();
        let ctx = customer_context("cust-1");
        let attrs = HashMap::new();

        let decision = engine.authorize(&ctx, "read", "Order", &attrs);
        assert_eq!(
            decision,
            AuthzDecision::Deny("no matching permit policy".to_string())
        );
    }

    #[test]
    fn test_system_bypass() {
        let engine = AuthzEngine::permissive();
        let ctx = SecurityContext::system();
        let attrs = HashMap::new();

        let decision = engine.authorize_or_bypass(&ctx, "read", "Order", &attrs);
        assert!(decision.is_allowed());
    }

    #[test]
    fn test_admin_permit_policy() {
        let policy = r#"
            permit(
                principal is Admin,
                action,
                resource
            );
        "#;

        let engine = AuthzEngine::new(policy).unwrap();
        let ctx = admin_context();
        let attrs = HashMap::new();

        let decision = engine.authorize(&ctx, "read", "Order", &attrs);
        assert!(
            decision.is_allowed(),
            "admin should be allowed, got: {decision:?}"
        );
    }

    #[test]
    fn test_customer_denied_without_matching_policy() {
        let policy = r#"
            permit(
                principal is Admin,
                action,
                resource
            );
        "#;

        let engine = AuthzEngine::new(policy).unwrap();
        let ctx = customer_context("cust-1");
        let attrs = HashMap::new();

        let decision = engine.authorize(&ctx, "read", "Order", &attrs);
        assert!(!decision.is_allowed(), "customer should be denied");
    }

    #[test]
    fn test_invalid_policy_returns_error() {
        let result = AuthzEngine::new("this is not valid cedar");
        assert!(result.is_err());
    }

    #[test]
    fn test_decision_is_allowed() {
        assert!(AuthzDecision::Allow.is_allowed());
        assert!(!AuthzDecision::Deny("reason".into()).is_allowed());
    }

    #[test]
    fn test_hot_reload_replaces_policies() {
        // Start with admin-only policy
        let admin_policy = r#"
            permit(
                principal is Admin,
                action,
                resource
            );
        "#;
        let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");
        assert_eq!(engine.policy_count(), 1);

        // Customer is denied
        let ctx = customer_context("cust-1");
        let attrs = HashMap::new();
        assert!(!engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());

        // Hot-reload to customer-permitting policy
        let customer_policy = r#"
            permit(
                principal is Customer,
                action,
                resource
            );
        "#;
        engine
            .reload_policies(customer_policy)
            .expect("reload should succeed");
        assert_eq!(engine.policy_count(), 1);

        // Now customer is allowed
        assert!(engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());

        // Admin is now denied (only customer policy active)
        let admin_ctx = admin_context();
        assert!(
            !engine
                .authorize(&admin_ctx, "read", "Order", &attrs)
                .is_allowed()
        );
    }

    #[test]
    fn test_hot_reload_invalid_preserves_existing() {
        let admin_policy = r#"
            permit(
                principal is Admin,
                action,
                resource
            );
        "#;
        let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");

        // Try to reload with invalid policy — should fail
        let result = engine.reload_policies("not valid cedar at all");
        assert!(result.is_err());

        // Original policy still works
        let ctx = admin_context();
        let attrs = HashMap::new();
        assert!(engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());
        assert_eq!(engine.policy_count(), 1);
    }

    #[test]
    fn test_hot_reload_to_empty() {
        let admin_policy = r#"
            permit(
                principal is Admin,
                action,
                resource
            );
        "#;
        let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");

        // Reload with empty policy set
        engine
            .reload_policies("")
            .expect("empty policy should parse");
        assert_eq!(engine.policy_count(), 0);

        // Admin is now denied (no policies)
        let ctx = admin_context();
        let attrs = HashMap::new();
        assert!(!engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());
    }

    #[test]
    fn test_context_entity_status_in_cedar_context() {
        // Policy that gates on context.ctx_parent_agent_status
        let policy = r#"
            permit(
                principal is Agent,
                action == Action::"canary_deploy",
                resource is DeployWorkflow
            ) when {
                context.ctx_parent_agent_status == "canary_ok"
            };
        "#;

        let engine = AuthzEngine::new(policy).unwrap();

        let ctx = SecurityContext::from_headers(&[
            ("x-temper-principal-id".to_string(), "agent-1".to_string()),
            ("x-temper-principal-kind".to_string(), "agent".to_string()),
        ]);

        // Without context entity status: should deny
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), serde_json::json!("deploy-1"));
        let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
        assert!(
            !decision.is_allowed(),
            "should deny without context entity status"
        );

        // With context entity status matching: should allow
        attrs.insert(
            "ctx_parent_agent_status".to_string(),
            serde_json::json!("canary_ok"),
        );
        let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
        assert!(
            decision.is_allowed(),
            "should allow with matching context entity status, got: {decision:?}"
        );

        // With wrong context entity status: should deny
        attrs.insert(
            "ctx_parent_agent_status".to_string(),
            serde_json::json!("planning"),
        );
        let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
        assert!(
            !decision.is_allowed(),
            "should deny with wrong context entity status"
        );
    }
}
