//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::HashMap;
use std::str::FromStr;

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
/// authorization requests.
pub struct AuthzEngine {
    policy_set: PolicySet,
    authorizer: Authorizer,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        let policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;

        Ok(Self {
            policy_set,
            authorizer: Authorizer::new(),
        })
    }

    /// Create an AuthzEngine with no policies (allows everything by default).
    pub fn permissive() -> Self {
        Self {
            policy_set: PolicySet::new(),
            authorizer: Authorizer::new(),
        }
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

        let principal_uid = match EntityUid::from_str(
            &format!("{}::\"{}\"", principal_type, security_ctx.principal.id),
        ) {
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
        let resource_uid = match EntityUid::from_str(
            &format!("{}::\"{}\"", resource_type, resource_id_from_attrs(resource_attrs)),
        ) {
            Ok(uid) => uid,
            Err(e) => {
                return AuthzDecision::Deny(format!("invalid resource: {e}"));
            }
        };

        // Build Cedar context from security context attrs + resource attrs
        let mut ctx_map: HashMap<String, cedar_policy::RestrictedExpression> = HashMap::new();

        // Add principal attributes to context
        if let Some(ref role) = security_ctx.principal.role {
            ctx_map.insert("role".to_string(), cedar_policy::RestrictedExpression::new_string(role.clone()));
        }
        if let Some(ref acting_for) = security_ctx.principal.acting_for {
            ctx_map.insert("actingFor".to_string(), cedar_policy::RestrictedExpression::new_string(acting_for.clone()));
        }

        // Add context attributes
        for (key, value) in &security_ctx.context_attrs {
            if let Some(s) = value.as_str() {
                ctx_map.insert(key.clone(), cedar_policy::RestrictedExpression::new_string(s.to_string()));
            } else if let Some(b) = value.as_bool() {
                ctx_map.insert(key.clone(), cedar_policy::RestrictedExpression::new_bool(b));
            }
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

        let response: CedarResponse = self.authorizer.is_authorized(&request, &self.policy_set, &entities);

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
            ("X-Temper-Principal-Kind".to_string(), "customer".to_string()),
        ])
    }

    fn agent_context(role: &str) -> SecurityContext {
        SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), "agent-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
            ("X-Temper-Agent-Role".to_string(), role.to_string()),
        ])
    }

    #[test]
    fn test_permissive_engine_denies_by_default() {
        // No policies = no permits = deny
        let engine = AuthzEngine::permissive();
        let ctx = customer_context("cust-1");
        let attrs = HashMap::new();

        let decision = engine.authorize(&ctx, "read", "Order", &attrs);
        assert_eq!(decision, AuthzDecision::Deny("no matching permit policy".to_string()));
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
        assert!(decision.is_allowed(), "admin should be allowed, got: {decision:?}");
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
}
