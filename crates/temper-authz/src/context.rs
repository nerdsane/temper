//! Security context — extracted from HTTP request, carried through actor dispatch.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use temper_runtime::tenant::TenantId;

/// The kind of principal making the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalKind {
    /// A human customer.
    Customer,
    /// An LLM agent acting on behalf of someone.
    Agent,
    /// A system administrator.
    Admin,
    /// An internal system process.
    System,
}

/// A principal (the entity making the request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// The principal's unique identifier.
    pub id: String,
    /// The kind of principal.
    pub kind: PrincipalKind,
    /// The agent's role (if kind is Agent): customer_agent, operations_agent, support_agent.
    pub role: Option<String>,
    /// If this agent is acting on behalf of another principal.
    pub acting_for: Option<String>,
    /// Agent type classification (e.g. "claude-code", "openclaw").
    pub agent_type: Option<String>,
    /// Arbitrary attributes for ABAC evaluation.
    pub attributes: HashMap<String, serde_json::Value>,
}

/// Cedar context keys that only a resolved `SecurityContext` may populate.
///
/// Resource attribute builders and `evaluate_request` treat these as reserved
/// the same way `id`/`status` are server-derived: a caller body field of the
/// same name must not become `context.sessionId` (or overwrite a grant-checked
/// value). Generated session, type, and role permits all condition on these.
pub fn is_cedar_authority_context_key(name: &str) -> bool {
    matches!(
        name,
        "sessionId" | "agentId" | "agentType" | "agentTypeVerified" | "role" | "actingFor"
    )
}

/// Security context carried with every actor message dispatch.
///
/// Protected HTTP requests receive this from credential resolution. The legacy
/// header constructor intentionally produces only anonymous authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContext {
    /// The principal making the request.
    pub principal: Principal,
    /// Additional context attributes (time, IP, rate limit flags, etc.)
    pub context_attrs: HashMap<String, serde_json::Value>,
    /// Correlation ID for tracing.
    pub correlation_id: String,
}

/// Credential-authenticated authority bound to one tenant.
///
/// Authentication middleware constructs this once after resolving a credential.
/// Downstream handlers receive the value as an axum extension or a direct
/// in-process argument. Private fields prevent handlers from replacing only the
/// tenant or only the principal and accidentally creating a mixed context.
#[derive(Debug, Clone)]
pub struct AuthenticatedRequestContext {
    tenant: TenantId,
    security_context: SecurityContext,
    /// Caller-declared intent for this request, for telemetry only.
    ///
    /// Deliberately NOT part of `security_context.context_attrs`: those are
    /// Cedar inputs, and this value is caller-supplied. A denial record without
    /// the intent behind it says what was blocked but not what the caller was
    /// trying to do — the half that drives policy proposals — so it is carried
    /// here, where authorization cannot read it.
    intent: Option<String>,
    /// Caller-declared session id for this request, for telemetry only.
    ///
    /// Held here for the same reason as `intent`, and the reason is sharper:
    /// Cedar policies condition on `context.sessionId` (session-scoped permits
    /// generated from an approved decision). Routing a caller-supplied header
    /// into `context_attrs` would let any caller satisfy the session scope that
    /// made such an approval narrow, replaying it indefinitely.
    ///
    /// The asserted header becomes a Cedar input only through the validated
    /// path: the bearer edge checks it against the server-side grant record (an
    /// approved decision binding that session to this principal) and only then
    /// passes it into the resolved `SecurityContext`. This field always carries
    /// the raw assertion for telemetry, validated or not.
    session_id: Option<String>,
}

impl AuthenticatedRequestContext {
    /// Bind an already-resolved security context to its authenticated tenant.
    pub fn new(tenant: TenantId, security_context: SecurityContext) -> Self {
        Self {
            tenant,
            security_context,
            intent: None,
            session_id: None,
        }
    }

    /// Attach the caller-declared intent from the correlation headers.
    #[must_use]
    pub fn with_intent(mut self, intent: Option<String>) -> Self {
        self.intent = intent;
        self
    }

    /// Caller-declared intent, for denial telemetry. Never an authorization input.
    pub fn intent(&self) -> Option<&str> {
        self.intent.as_deref()
    }

    /// Attach the caller-declared session id from the correlation headers.
    #[must_use]
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Caller-declared session id, for telemetry. Never an authorization input.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Tenant selected during credential resolution.
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Exact security context produced by credential resolution.
    pub fn security_context(&self) -> &SecurityContext {
        &self.security_context
    }
}

impl SecurityContext {
    /// The anonymous principal used for routes declared public.
    ///
    /// Explicitly constructed rather than "derived from no headers", so no
    /// production path expresses identity as a function of request headers.
    pub fn anonymous() -> Self {
        SecurityContext {
            principal: Principal {
                id: "anonymous".to_string(),
                kind: PrincipalKind::Customer,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: HashMap::new(),
            },
            context_attrs: HashMap::new(),
            correlation_id: uuid::Uuid::now_v7().to_string(),
        }
    }

    /// Build a context from request headers.
    ///
    /// Test-only since ADR-0157: identity comes from a resolved credential, and
    /// keeping this out of the production build makes header-derived identity
    /// impossible to reintroduce by accident rather than merely unused.
    #[cfg(test)]
    pub fn from_headers(headers: &[(String, String)]) -> Self {
        let mut correlation_id = uuid::Uuid::now_v7().to_string();

        for (key, value) in headers {
            if key.eq_ignore_ascii_case("x-temper-correlation-id") {
                correlation_id = value.clone();
            }
        }

        SecurityContext {
            principal: Principal {
                id: "anonymous".to_string(),
                kind: PrincipalKind::Customer,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: HashMap::new(),
            },
            context_attrs: HashMap::new(),
            correlation_id,
        }
    }

    /// Create a system-level security context for explicit in-process work.
    ///
    /// System requests still pass Cedar evaluation through the built-in system
    /// policy; this constructor is never reachable from HTTP headers.
    pub fn system() -> Self {
        SecurityContext {
            principal: Principal {
                id: "system".to_string(),
                kind: PrincipalKind::System,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: HashMap::new(),
            },
            context_attrs: HashMap::new(),
            correlation_id: uuid::Uuid::now_v7().to_string(),
        }
    }

    /// Construct security context from a platform-resolved agent identity.
    ///
    /// All identity fields come from the credential registry — never from
    /// self-declared headers. Sets `agentTypeVerified = true`.
    ///
    /// See ADR-0033: Platform-Assigned Agent Identity.
    pub fn from_resolved_identity(
        agent_instance_id: &str,
        agent_type_name: &str,
        session_id: Option<&str>,
    ) -> Self {
        let mut attributes = HashMap::new();
        attributes.insert(
            "agentTypeVerified".to_string(),
            serde_json::Value::Bool(true),
        );

        let mut context_attrs = HashMap::new();
        context_attrs.insert(
            "agentId".to_string(),
            serde_json::Value::String(agent_instance_id.to_string()),
        );
        context_attrs.insert(
            "agentType".to_string(),
            serde_json::Value::String(agent_type_name.to_string()),
        );
        context_attrs.insert(
            "agentTypeVerified".to_string(),
            serde_json::Value::Bool(true),
        );
        if let Some(sid) = session_id {
            context_attrs.insert(
                "sessionId".to_string(),
                serde_json::Value::String(sid.to_string()),
            );
        }

        SecurityContext {
            principal: Principal {
                id: agent_instance_id.to_string(),
                kind: PrincipalKind::Agent,
                role: None,
                acting_for: None,
                agent_type: Some(agent_type_name.to_string()),
                attributes,
            },
            context_attrs,
            correlation_id: uuid::Uuid::now_v7().to_string(),
        }
    }

    /// Enrich an internal context with an explicitly unverified agent identity.
    ///
    /// **Deprecated**: Use `from_resolved_identity()` for credential-based
    /// identity. This compatibility helper is limited to legacy internal
    /// trigger/service construction; callers must never pass raw HTTP values.
    pub fn with_agent_context(
        mut self,
        agent_id: Option<&str>,
        session_id: Option<&str>,
        agent_type: Option<&str>,
    ) -> Self {
        if let Some(aid) = agent_id {
            self.context_attrs.insert(
                "agentId".to_string(),
                serde_json::Value::String(aid.to_string()),
            );
            // Promote anonymous principals to Agent kind
            if self.principal.id == "anonymous" {
                self.principal.id = aid.to_string();
                self.principal.kind = PrincipalKind::Agent;
                if self.principal.role.is_none() {
                    self.principal.role = Some("wasm_module".to_string());
                }
            }
        }
        if let Some(sid) = session_id {
            self.context_attrs.insert(
                "sessionId".to_string(),
                serde_json::Value::String(sid.to_string()),
            );
        }
        if let Some(at) = agent_type {
            self.context_attrs.insert(
                "agentType".to_string(),
                serde_json::Value::String(at.to_string()),
            );
            self.principal.agent_type = Some(at.to_string());
        }
        // Mark as unverified — identity is self-declared, not credential-resolved.
        self.principal.attributes.insert(
            "agentTypeVerified".to_string(),
            serde_json::Value::Bool(false),
        );
        self.context_attrs.insert(
            "agentTypeVerified".to_string(),
            serde_json::Value::Bool(false),
        );
        self
    }

    /// Attach ADR-0040 action-context provenance to the principal entity.
    ///
    /// Cedar policies can then match on `principal.action_context`, for
    /// example `principal.action_context == "composite:Apps.Fork"`.
    pub fn with_action_context(mut self, action_context: impl Into<String>) -> Self {
        self.principal.attributes.insert(
            "action_context".to_string(),
            serde_json::Value::String(action_context.into()),
        );
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn customer_headers_are_non_authoritative() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ];

        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.id, "anonymous");
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert!(ctx.principal.role.is_none());
    }

    #[test]
    fn correlation_header_does_not_restore_authority() {
        let ctx = SecurityContext::from_headers(&[
            ("x-temper-correlation-id".to_string(), "trace-1".to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ]);

        assert_eq!(ctx.correlation_id, "trace-1");
        assert_eq!(ctx.principal.id, "anonymous");
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
    }

    #[test]
    fn agent_role_and_delegation_headers_are_non_authoritative() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "agent-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
            (
                "X-Temper-Agent-Role".to_string(),
                "customer_agent".to_string(),
            ),
            ("X-Temper-Acting-For".to_string(), "cust-456".to_string()),
        ];

        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.id, "anonymous");
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert!(ctx.principal.role.is_none());
        assert!(ctx.principal.acting_for.is_none());
    }

    #[test]
    fn test_context_from_headers_with_attributes() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "admin-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
            (
                "X-Temper-Attr-ApprovalLimit".to_string(),
                "10000".to_string(),
            ),
            (
                "X-Temper-Ctx-RateLimitExceeded".to_string(),
                "false".to_string(),
            ),
        ];

        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert!(ctx.principal.attributes.is_empty());
        assert!(ctx.context_attrs.is_empty());
    }

    #[test]
    fn no_header_marker_can_mint_admin() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "attacker".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
            (
                "x-temper-internal-trusted-principal".to_string(),
                "1".to_string(),
            ),
        ];
        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert_eq!(ctx.principal.id, "anonymous");
    }

    #[test]
    fn caller_declared_correlation_never_becomes_a_cedar_input() {
        // Cedar's context is built from `SecurityContext::context_attrs`. Session
        // id and intent are caller-supplied headers, and policies condition on
        // `context.sessionId` (session-scoped permits minted from an approved
        // decision), so putting either in `context_attrs` would let any caller
        // satisfy the scope that made such an approval narrow.
        let authenticated = AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        )
        .with_session_id(Some("sess-approved".to_string()))
        .with_intent(Some("delete everything".to_string()));

        assert_eq!(authenticated.session_id(), Some("sess-approved"));
        assert_eq!(authenticated.intent(), Some("delete everything"));

        let attrs = &authenticated.security_context().context_attrs;
        assert!(
            attrs.get("sessionId").is_none(),
            "a caller-supplied session id must not reach the Cedar context"
        );
        assert!(
            attrs.get("intent").is_none(),
            "caller-supplied intent must not reach the Cedar context"
        );
    }

    #[test]
    fn system_principal_never_derivable_from_headers() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "svc".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "system".to_string()),
        ];
        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
    }

    #[test]
    fn authenticated_context_binds_exact_tenant_and_security_context() {
        let security_context =
            SecurityContext::from_resolved_identity("agent-1", "operator", Some("session-1"));
        let authenticated =
            AuthenticatedRequestContext::new(TenantId::new("tenant-a"), security_context.clone());

        assert_eq!(authenticated.tenant().as_str(), "tenant-a");
        assert_eq!(
            authenticated.security_context().principal.id,
            security_context.principal.id
        );
        assert_eq!(
            authenticated.security_context().context_attrs,
            security_context.context_attrs
        );
    }

    #[test]
    fn context_attribute_headers_are_non_authoritative() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "agent-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
            (
                "x-temper-ctx-sessionid".to_string(),
                "session-1".to_string(),
            ),
        ];

        let ctx = SecurityContext::from_headers(&headers);
        assert!(ctx.context_attrs.is_empty());
    }

    #[test]
    fn test_system_context() {
        let ctx = SecurityContext::system();
        assert_eq!(ctx.principal.id, "system");
        assert_eq!(ctx.principal.kind, PrincipalKind::System);
    }

    #[test]
    fn test_with_agent_context_promotes_anonymous() {
        let ctx = SecurityContext::from_headers(&[]).with_agent_context(
            Some("stripe_charge"),
            Some("sess-1"),
            None,
        );

        assert_eq!(ctx.principal.id, "stripe_charge");
        assert_eq!(ctx.principal.kind, PrincipalKind::Agent);
        assert_eq!(ctx.principal.role, Some("wasm_module".to_string()));
        assert_eq!(
            ctx.context_attrs.get("agentId"),
            Some(&serde_json::Value::String("stripe_charge".to_string()))
        );
        assert_eq!(
            ctx.context_attrs.get("sessionId"),
            Some(&serde_json::Value::String("sess-1".to_string()))
        );
    }

    #[test]
    fn explicit_agent_context_promotes_header_anonymous_context() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ];
        let ctx =
            SecurityContext::from_headers(&headers).with_agent_context(Some("agent-1"), None, None);

        assert_eq!(ctx.principal.id, "agent-1");
        assert_eq!(ctx.principal.kind, PrincipalKind::Agent);
        assert_eq!(
            ctx.context_attrs.get("agentId"),
            Some(&serde_json::Value::String("agent-1".to_string()))
        );
    }

    #[test]
    fn cedar_authority_context_keys_are_reserved() {
        assert!(is_cedar_authority_context_key("sessionId"));
        assert!(is_cedar_authority_context_key("agentTypeVerified"));
        assert!(is_cedar_authority_context_key("role"));
        assert!(!is_cedar_authority_context_key("status"));
        assert!(!is_cedar_authority_context_key("Customer"));
    }

    #[test]
    fn system_principal_cannot_be_spoofed_via_headers() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "attacker".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "system".to_string()),
        ];
        let ctx = SecurityContext::from_headers(&headers);
        // Must NOT be System — falls back to Customer.
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert_eq!(ctx.principal.id, "anonymous");
    }

    #[test]
    fn test_with_agent_context_none_values() {
        let ctx = SecurityContext::system().with_agent_context(None, None, None);
        assert_eq!(ctx.principal.id, "system");
        assert!(!ctx.context_attrs.contains_key("agentId"));
        assert!(!ctx.context_attrs.contains_key("sessionId"));
    }

    #[test]
    fn test_from_headers_with_agent_type() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "bot-1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
            ("X-Temper-Agent-Type".to_string(), "claude-code".to_string()),
        ];
        let ctx = SecurityContext::from_headers(&headers);
        assert!(ctx.principal.agent_type.is_none());
    }

    #[test]
    fn test_from_headers_with_principal_scopes() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
            (
                "X-Temper-Principal-Scopes".to_string(),
                "repo:read,repo:write force".to_string(),
            ),
        ];
        let ctx = SecurityContext::from_headers(&headers);
        assert!(!ctx.principal.attributes.contains_key("scopes"));
    }

    #[test]
    fn action_context_header_is_ignored_but_explicit_context_is_preserved() {
        let ctx = SecurityContext::from_headers(&[(
            "x-temper-action-context".to_string(),
            "composite:Apps.Fork".to_string(),
        )]);

        assert!(!ctx.principal.attributes.contains_key("action_context"));

        let ctx = SecurityContext::from_headers(&[]).with_action_context("composite:Repo.Write");
        assert_eq!(
            ctx.principal.attributes.get("action_context"),
            Some(&serde_json::Value::String(
                "composite:Repo.Write".to_string()
            ))
        );
    }
}
