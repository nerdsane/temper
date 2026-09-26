//! Authorization is independent of the request thread's remaining stack.
use super::*;

fn read_policy(effect: &str, resource_types: usize) -> String {
    let types = (0..resource_types)
        .map(|index| format!("resource is Resource{index}"))
        .collect::<Vec<_>>()
        .join(" || ");
    format!("{effect}(principal, action == Action::\"read\", resource) when {{ {types} }};")
}

fn evaluate_on_request_stack(policy: &str) -> AuthzDecision {
    // Install on a separate stack so this probe isolates evaluation depth.
    let engine = AuthzEngine::empty();
    stacker::grow(32 * 1024 * 1024, || {
        engine.reload_tenant_policies("stack-test", policy).unwrap();
    });
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            let context = SecurityContext::from_resolved_identity("agent", "worker", None);
            engine.authorize_for_tenant(
                "stack-test",
                &context,
                "read",
                "Resource0",
                &HashMap::new(),
            )
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn a_matching_permit_survives_a_small_request_stack() {
    let decision = evaluate_on_request_stack(&read_policy("permit", 40));
    assert!(decision.is_allowed(), "{decision:?}");
}

#[test]
fn a_matching_forbid_is_not_skipped_on_a_small_request_stack() {
    let policies = format!(
        "permit(principal, action, resource);\n{}",
        read_policy("forbid", 40)
    );
    let decision = evaluate_on_request_stack(&policies);
    assert!(
        matches!(
            decision,
            AuthzDecision::Deny(AuthzDenial::PolicyDenied { .. })
        ),
        "{decision:?}"
    );
}

#[test]
fn a_forbid_beyond_the_evaluation_stack_budget_fails_closed() {
    // Frame sizes depend on how Cedar is compiled. With optimized dependencies
    // the 8 MiB evaluation budget holds between 1400 and 2000 disjuncts; in an
    // unoptimized build the 32 MiB install stack overflows parsing 8000. 4000
    // exceeds the budget under both with room to spare.
    let policies = format!(
        "permit(principal, action, resource);\n{}",
        read_policy("forbid", 4000)
    );
    let decision = evaluate_on_request_stack(&policies);
    assert_eq!(
        decision,
        AuthzDecision::Deny(AuthzDenial::EngineError(
            "Cedar policy evaluation exceeded its stack limit".into()
        ))
    );
}
