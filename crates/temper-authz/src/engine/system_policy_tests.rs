//! Sharing the parsed built-in source must not share active authorization state.

use super::*;

#[test]
fn cached_system_policy_preserves_all_statements_and_independent_clones() {
    let cached = system_platform_policies().expect("built-in policy should parse");
    let fresh: PolicySet = system_platform_policy().parse().unwrap();
    assert_eq!(cached, &fresh);

    let first_id = cached.policies().next().unwrap().id().clone();
    let mut local = cached.clone();
    local.remove_static(first_id.clone()).unwrap();
    assert!(local.policy(&first_id).is_none());

    let after = system_platform_policies().unwrap();
    assert!(std::ptr::eq(cached, after), "reuse the immutable parse");
    assert_eq!(after, &fresh, "local mutation must not alter the cache");
}

#[test]
fn cached_system_policy_keeps_reloads_isolated_across_engines_and_tenants() {
    let first = AuthzEngine::empty();
    let second = AuthzEngine::empty();
    let agent = SecurityContext::from_resolved_identity("reader", "worker", None);
    let system = SecurityContext::system();
    let attrs = HashMap::new();
    let permit_agent = r#"permit(principal is Agent, action == Action::"read", resource is Doc);"#;

    first.reload_tenant_policies("a", permit_agent).unwrap();
    first
        .reload_tenant_policies_named(
            "b",
            &[(
                "system-read-denied".to_string(),
                r#"forbid(principal is System, action == Action::"read", resource is Doc);"#
                    .to_string(),
            )],
        )
        .unwrap();
    second
        .reload_tenant_policies_named(
            "a",
            &[(
                "agent-read-denied".to_string(),
                r#"forbid(principal is Agent, action == Action::"read", resource is Doc);"#
                    .to_string(),
            )],
        )
        .unwrap();
    second.reload_tenant_policies("b", permit_agent).unwrap();

    assert!(
        first
            .authorize_for_tenant("a", &agent, "read", "Doc", &attrs)
            .is_allowed()
    );
    assert_eq!(
        first.authorize_for_tenant("b", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
    );
    assert!(matches!(
        second.authorize_for_tenant("a", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids })
            if policy_ids == ["a:agent-read-denied"],
    ));
    assert!(
        second
            .authorize_for_tenant("b", &agent, "read", "Doc", &attrs)
            .is_allowed()
    );
    assert!(matches!(
        first.authorize_for_tenant("b", &system, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids })
            if policy_ids == ["b:system-read-denied"],
    ));
    assert!(
        second
            .authorize_for_tenant("b", &system, "read", "Doc", &attrs)
            .is_allowed()
    );

    assert!(first.reload_tenant_policies("a", "not cedar").is_err());
    assert!(
        first
            .authorize_for_tenant("a", &agent, "read", "Doc", &attrs)
            .is_allowed()
    );
    first.reload_tenant_policies("a", "").unwrap();
    assert_eq!(
        first.authorize_for_tenant("a", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
    );
    assert!(
        first
            .authorize_for_tenant("a", &system, "read", "Doc", &attrs)
            .is_allowed()
    );
    assert!(
        second
            .authorize_for_tenant("b", &agent, "read", "Doc", &attrs)
            .is_allowed()
    );

    // Later engines must still start with the full built-in policy and no tenant permits.
    let later = AuthzEngine::empty();
    for engine in [&first, &second, &later] {
        assert_eq!(
            engine.authorize_for_tenant("unloaded", &agent, "read", "Doc", &attrs),
            AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
        );
        assert!(
            engine
                .authorize_for_tenant("unloaded", &system, "read", "Doc", &attrs)
                .is_allowed()
        );
    }
}
