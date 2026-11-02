use super::*;
use crate::context::{Principal, PrincipalKind};

fn principal_of_kind(kind: PrincipalKind) -> SecurityContext {
    let mut ctx = SecurityContext::anonymous();
    ctx.principal = Principal {
        id: "p-1".to_string(),
        kind,
        role: None,
        acting_for: None,
        agent_type: None,
        label: None,
        attributes: HashMap::new(),
    };
    ctx
}

/// The engine's answer follows its policies, not a permit-all fallback: a
/// permissive engine still says an ordinary agent has no platform authority.
#[test]
fn platform_authority_is_system_admin_or_a_verified_operator() {
    for engine in [AuthzEngine::empty(), AuthzEngine::permissive()] {
        assert!(engine.has_platform_authority(&SecurityContext::system()));
        assert!(engine.has_platform_authority(&principal_of_kind(PrincipalKind::Admin)));
        assert!(
            engine.has_platform_authority(&SecurityContext::from_resolved_identity(
                "op-1", "operator", None
            ))
        );

        assert!(!engine.has_platform_authority(&principal_of_kind(PrincipalKind::Customer)));
        assert!(
            !engine.has_platform_authority(&SecurityContext::from_resolved_identity(
                "worker-1", "worker", None
            ))
        );
        let self_declared_operator =
            SecurityContext::anonymous().with_agent_context(Some("op-2"), None, Some("operator"));
        assert!(!engine.has_platform_authority(&self_declared_operator));
    }
}
