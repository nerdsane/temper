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
        let mut kind = PrincipalKind::System;
        let mut role = None;
        let mut acting_for = None;
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
                        _ => PrincipalKind::System,
                    };
                }
                "x-temper-agent-role" => role = Some(value.clone()),
                "x-temper-acting-for" => acting_for = Some(value.clone()),
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
                attributes: HashMap::new(),
            },
            context_attrs: HashMap::new(),
            correlation_id: uuid::Uuid::now_v7().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_from_headers_customer() {
        let headers = vec![
            ("X-Temper-Principal-Id".to_string(), "cust-123".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "customer".to_string()),
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
            ("X-Temper-Agent-Role".to_string(), "customer_agent".to_string()),
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
            ("X-Temper-Attr-ApprovalLimit".to_string(), "10000".to_string()),
            ("X-Temper-Ctx-RateLimitExceeded".to_string(), "false".to_string()),
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
}
