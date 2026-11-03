//! Who an integration's triggering move was made as.

use temper_authz::PrincipalKind;
use temper_wasm::TriggerPrincipal;

use crate::request_context::AgentContext;

/// The principal `agent`, the context the triggering move was dispatched
/// under, names: the one Cedar saw for that move. A timer's move carries its
/// setter's saved context and a reaction's the mover's it reacts to, so their
/// integrations see that setter or mover. None for a context that names no
/// caller ([`AgentContext::caller`]): a system or service move, an anonymous
/// one, or one without a security context.
pub(super) fn trigger_principal(agent: &AgentContext) -> Option<TriggerPrincipal> {
    let principal = &agent.caller()?.principal;
    let kind = match principal.kind {
        PrincipalKind::Customer => "Customer",
        PrincipalKind::Agent => "Agent",
        PrincipalKind::Admin => "Admin",
        // A System principal names no caller, so `caller` already left it out.
        PrincipalKind::System => return None,
    };
    let present = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    Some(TriggerPrincipal {
        id: principal.id.clone(),
        kind: kind.to_string(),
        acting_for: present(&principal.acting_for),
        agent_type: present(&principal.agent_type),
    })
}
