//! Reaction target principal resolution.

use crate::request_context::AgentContext;

/// Resolve an explicit reaction service principal or inherit the caller.
pub(super) fn resolve_trigger_principal(
    declared_principal: Option<&str>,
    invoking_ctx: &AgentContext,
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
) -> AgentContext {
    match declared_principal {
        Some(service_name) if !service_name.is_empty() => {
            let mut ctx = AgentContext::for_service_inheriting(service_name, invoking_ctx);
            // Preserve ADR-0048 behavior for declared reaction principals:
            // existing trigger dispatch copied the caller's idempotency key.
            ctx.idempotency_key = invoking_ctx.idempotency_key.clone();
            if let Some(security_ctx) = ctx.security_ctx.as_mut() {
                security_ctx.context_attrs.insert(
                    "triggerRule".to_string(),
                    serde_json::Value::String(rule_name.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityType".to_string(),
                    serde_json::Value::String(source_entity_type.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityId".to_string(),
                    serde_json::Value::String(source_entity_id.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceAction".to_string(),
                    serde_json::Value::String(source_action.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerDeclaredPrincipal".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            ctx
        }
        _ => invoking_ctx.clone(),
    }
}
