//! Security context — extracted from HTTP request, carried through actor dispatch.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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

/// Security context carried with every actor message dispatch.
/// Constructed from HTTP request headers at the server boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContext {
    /// The principal making the request.
    pub principal: Principal,
    /// Additional context attributes (time, IP, rate limit flags, etc.)
    pub context_attrs: HashMap<String, serde_json::Value>,
    /// Correlation ID for tracing.
    pub correlation_id: String,
}

impl SecurityContext {
    /// Create a security context from HTTP request headers.
    /// In production, this would validate JWT tokens, API keys, etc.
    /// For now, extracts from X-Temper-* headers.
    pub fn from_headers(headers: &[(String, String)]) -> Self {
        let mut principal_id = "anonymous".to_string();
        // Default to Customer (most restrictive). System bypass is only via
        // SecurityContext::system() or explicit "system" header value.
        let mut kind = PrincipalKind::Customer;
        let mut role = None;
        let mut acting_for = None;
        let mut agent_type = None;
        let mut attributes = HashMap::new();
        let mut context_attrs = HashMap::new();
        let mut correlation_id = uuid::Uuid::now_v7().to_string();

        for (key, value) in headers {
            match key.to_lowercase().as_str() {
                "x-temper-principal-id" => principal_id = value.clone(),
                "x-temper-principal-kind" => {
                    kind = match value.as_str() {
                        "customer" => PrincipalKind::Customer,
                        "agent" => PrincipalKind::Agent,
                        "admin" => PrincipalKind::Admin,
                        // "system" is NOT accepted from headers to prevent
                        // privilege escalation via header spoofing.  Use
                        // SecurityContext::system() for trusted internal paths.
                        _ => PrincipalKind::Customer,
                    };
                }
                "x-temper-agent-role" => role = Some(value.clone()),
                "x-temper-acting-for" => acting_for = Some(value.clone()),
                "x-temper-agent-type" => agent_type = Some(value.clone()),
                "x-temper-correlation-id" => correlation_id = value.clone(),
                k if k.starts_with("x-temper-attr-") => {
                    let attr_name = k.strip_prefix("x-temper-attr-").unwrap(); // ci-ok: guarded by starts_with
                    attributes.insert(
                        attr_name.to_string(),
                        serde_json::Value::String(value.clone()),
                    );
                }
                k if k.starts_with("x-temper-ctx-") => {
                    let ctx_name = k.strip_prefix("x-temper-ctx-").unwrap(); // ci-ok: guarded by starts_with
                    context_attrs.insert(
                        ctx_name.to_string(),
                        serde_json::Value::String(value.clone()),
                    );
                }
                _ => {}
            }
        }

        SecurityContext {
            principal: Principal {
                id: principal_id,
                kind,
                role,
                acting_for,
                agent_type,
                attributes,
            },
            context_attrs,
            correlation_id,
        }
    }

    /// Create a system-level security context (bypasses all checks).
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

    /// Enrich security context with agent identity from self-declared headers.
    ///
    /// **Deprecated**: Use `from_resolved_identity()` for credential-based identity.
    /// This method is retained only for the global API key path (admin/operator access)
    /// where no agent credential exists.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_from_headers_customer() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ];

        let ctx = SecurityContext::from_headers(&headers);
        assert_eq!(ctx.principal.id, "cust-123");
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        assert!(ctx.principal.role.is_none());
    }

    #[test]
    fn test_context_from_headers_agent() {
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
        assert_eq!(ctx.principal.kind, PrincipalKind::Agent);
        assert_eq!(ctx.principal.role, Some("customer_agent".to_string()));
        assert_eq!(ctx.principal.acting_for, Some("cust-456".to_string()));
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
        assert_eq!(ctx.principal.kind, PrincipalKind::Admin);
        assert!(ctx.principal.attributes.contains_key("approvallimit"));
        assert!(ctx.context_attrs.contains_key("ratelimitexceeded"));
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
    fn test_with_agent_context_preserves_explicit_principal() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ];
        let ctx =
            SecurityContext::from_headers(&headers).with_agent_context(Some("agent-1"), None, None);

        // Should NOT overwrite explicit customer principal
        assert_eq!(ctx.principal.id, "cust-123");
        assert_eq!(ctx.principal.kind, PrincipalKind::Customer);
        // But agentId should be in context attrs
        assert_eq!(
            ctx.context_attrs.get("agentId"),
            Some(&serde_json::Value::String("agent-1".to_string()))
        );
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
        assert_eq!(ctx.principal.id, "attacker");
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
        assert_eq!(ctx.principal.agent_type, Some("claude-code".to_string()));
    }
}
