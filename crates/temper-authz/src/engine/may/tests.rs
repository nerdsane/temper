use super::*;
use crate::{AuthzEngine, SecurityContext};
use std::collections::HashMap;

#[test]
fn cedar_set_equality_denies_the_same_reaction_as_the_real_engine() {
    for (expression, denied) in [
        ("[1, 2] == [2, 1]", true),
        ("[1, 2, 1] == [2, 1]", true),
        ("[[1, 2], [3]] == [[3, 3], [2, 1, 1]]", true),
        ("[1, 2] != [2, 1]", false),
        ("[1, 2] == [1, 3]", false),
        ("[] == []", true),
    ] {
        let policies = format!(
            r#"permit(principal, action, resource);
            forbid(principal, action == Action::"Bring", resource is Place)
            when {{ {expression} }};"#
        );
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        let asker = Asker::Service {
            name: "place".into(),
            context: BTreeMap::new(),
            acting_for: false,
        };
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        let context = SecurityContext::from_resolved_identity("service:place", "place", None);
        let decision = engine.authorize(&context, "Bring", "Place", &HashMap::new());
        assert_eq!(decision.is_allowed(), !denied, "{expression}: {decision:?}");
        assert_eq!(
            analysis.may(&asker, "Bring", "Place", &BTreeMap::new()),
            if denied { May::No } else { May::Yes },
            "{expression}"
        );
    }
}

/// The kernel acting for a user a record names (a reaction's `as`) has no
/// service role and some `actingFor`: a policy on either is answered as the
/// engine answers the real context, or left open where the user decides it.
#[test]
fn a_service_acting_for_a_user_is_asked_as_the_engine_sees_it() {
    let asker = Asker::Service {
        name: "reaction".into(),
        context: BTreeMap::new(),
        acting_for: true,
    };
    // As the trigger dispatcher builds it: `AgentContext::for_service`, then
    // acting for the user, with no role.
    let mut real = SecurityContext::anonymous().with_agent_context(
        Some("service:reaction"),
        None,
        Some("reaction"),
    );
    real.principal.acting_for = Some("ann".to_string());
    real.principal.role = None;
    for (condition, real_denied, may) in [
        // Its role is not a service's.
        (
            r#"principal has role && principal.role == "service""#,
            false,
            May::Yes,
        ),
        // It acts for someone: which one, only the request knows.
        (r#"context has actingFor"#, true, May::No),
        (r#"context.actingFor == "ann""#, true, May::Maybe),
    ] {
        let policies = format!(
            r#"permit(principal, action, resource);
            forbid(principal, action == Action::"Start", resource is Run)
            when {{ {condition} }};"#
        );
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        let decision = engine.authorize(&real, "Start", "Run", &HashMap::new());
        assert_eq!(
            decision.is_allowed(),
            !real_denied,
            "{condition}: {decision:?}"
        );
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        assert_eq!(
            analysis.may(&asker, "Start", "Run", &BTreeMap::new()),
            may,
            "{condition}"
        );
    }
}
