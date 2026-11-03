//! Who makes a carry's `then` move, and whether the version's Cedar lets
//! them: the same question for the install's dry run and the carry pass.

use std::collections::BTreeMap;

use serde_json::Value;
use temper_authz::{AuthzDecision, AuthzEngine, platform_keys};
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;

/// The service that makes a carry's `then` move: Cedar sees the principal
/// `Agent::"service:carry"`.
const CARRY_SERVICE: &str = "carry";

/// Who makes the `then` move of the carry `carry_move` on a record holding
/// `fields`: the carry service, with `context.carryMove` naming the carry,
/// acting for the user the carry's `as` field names when it has one. `None`
/// when that field is empty: the move is not made, and the record stays
/// where the carry put it.
pub(super) fn then_maker(
    carry_move: &str,
    acts_for: Option<&str>,
    fields: &Value,
) -> Option<AgentContext> {
    let user = match acts_for {
        Some(field) => match fields.get(field).and_then(Value::as_str) {
            Some(user) if !user.is_empty() => Some(user),
            _ => return None,
        },
        None => None,
    };
    let mut maker = AgentContext::for_service(CARRY_SERVICE);
    let security = maker
        .security_ctx
        .as_mut()
        .expect("a service context carries its security context");
    security.context_attrs.insert(
        platform_keys::CARRY_MOVE.to_string(),
        Value::String(carry_move.to_string()),
    );
    if let Some(user) = user {
        // Acting for a user, the service has a subject, so a parameter bound
        // to the caller's identity is bound to that user's.
        security.principal.acting_for = Some(user.to_string());
        security.principal.role = None;
    }
    Some(maker)
}

/// Whether `cedar` lets `maker` make `action` on the record whose Cedar
/// attributes are `resource`, and why not.
pub(super) fn cedar_lets(
    cedar: &AuthzEngine,
    tenant: &TenantId,
    maker: &AgentContext,
    action: &str,
    entity_type: &str,
    resource: &BTreeMap<String, Value>,
) -> Result<(), String> {
    let security = maker
        .security_ctx
        .as_ref()
        .expect("a then move's maker carries its security context");
    let resource = resource.clone().into_iter().collect(); // determinism-ok: Cedar API requires HashMap
    match cedar.authorize_for_tenant_or_bypass(
        tenant.as_str(),
        security,
        action,
        entity_type,
        &resource,
    ) {
        AuthzDecision::Allow { .. } => Ok(()),
        AuthzDecision::Deny(denial) => Err(denial.to_string()),
    }
}
