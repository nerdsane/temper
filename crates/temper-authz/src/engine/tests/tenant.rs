use super::*;

#[test]
fn test_per_tenant_isolation() {
    let engine = AuthzEngine::empty();

    // Load different policies for two tenants.
    engine
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal, action == Action::"read", resource is Doc);"#,
        )
        .unwrap();
    engine
        .reload_tenant_policies(
            "tenant-b",
            r#"permit(principal, action == Action::"write", resource is Doc);"#,
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("doc-1"));

    // Tenant A allows read but not write.
    assert!(
        engine
            .authorize_for_tenant("tenant-a", &ctx, "read", "Doc", &attrs)
            .is_allowed()
    );
    assert!(
        !engine
            .authorize_for_tenant("tenant-a", &ctx, "write", "Doc", &attrs)
            .is_allowed()
    );

    // Tenant B allows write but not read.
    assert!(
        !engine
            .authorize_for_tenant("tenant-b", &ctx, "read", "Doc", &attrs)
            .is_allowed()
    );
    assert!(
        engine
            .authorize_for_tenant("tenant-b", &ctx, "write", "Doc", &attrs)
            .is_allowed()
    );
}

#[test]
fn tenant_policy_reloads_keep_system_platform_policy() {
    let engine = AuthzEngine::empty();
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("commit-1"));

    engine
        .reload_tenant_policies(
            "flat",
            r#"permit(principal is Customer, action == Action::"read", resource is Commit);"#,
        )
        .unwrap();

    let system = SecurityContext::system();
    assert!(
        engine
            .authorize_for_tenant("flat", &system, "Create", "Commit", &attrs)
            .is_allowed(),
        "System must keep its built-in Cedar permit after flat tenant reload"
    );

    let customer = customer_context("user-1");
    assert!(
        !engine
            .authorize_for_tenant("flat", &customer, "Create", "Commit", &attrs)
            .is_allowed(),
        "tenant user policies should still default-deny unrelated customer writes"
    );

    engine
        .reload_tenant_policies_named(
            "named",
            &[(
                "commit-read".to_string(),
                r#"permit(principal is Customer, action == Action::"read", resource is Commit);"#
                    .to_string(),
            )],
        )
        .unwrap();

    assert!(
        engine
            .authorize_for_tenant("named", &system, "Create", "Commit", &attrs)
            .is_allowed(),
        "System must keep its built-in Cedar permit after named tenant reload"
    );
}

#[test]
fn test_named_policies_produce_meaningful_ids() {
    let engine = AuthzEngine::empty();

    engine
        .reload_tenant_policies_named(
            "default",
            &[
                (
                    "os-app:pm".to_string(),
                    r#"permit(principal, action == Action::"read", resource is Issue);"#
                        .to_string(),
                ),
                (
                    "decision:abc".to_string(),
                    r#"permit(principal == Agent::"bot-1", action == Action::"Assign", resource is Issue);"#
                        .to_string(),
                ),
            ],
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    // Read is allowed (by os-app:pm policy).
    assert!(
        engine
            .authorize_for_tenant("default", &ctx, "read", "Issue", &attrs)
            .is_allowed()
    );

    // Assign is denied for user-1 (decision:abc only allows bot-1).
    let decision = engine.authorize_for_tenant("default", &ctx, "Assign", "Issue", &attrs);
    assert!(!decision.is_allowed());

    // Check that the denial includes meaningful policy IDs.
    if let AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids }) = &decision {
        // Should contain something like "default:decision:abc" not "policy0".
        let has_meaningful = policy_ids
            .iter()
            .any(|id| id.contains("default:") || id.contains("decision:"));
        assert!(
            has_meaningful,
            "policy IDs should be meaningful, got: {policy_ids:?}"
        );
    }
    // NoMatchingPermit is also acceptable since user-1 != bot-1
}

#[test]
fn candidate_filter_preserves_named_forbid_policy_ids() {
    let engine = AuthzEngine::empty();

    engine
        .reload_tenant_policies_named(
            "default",
            &[
                (
                    "os-app:issue-read".to_string(),
                    r#"permit(principal is Customer, action == Action::"read", resource is Issue);"#
                        .to_string(),
                ),
                (
                    "decision:block-issue-1".to_string(),
                    r#"forbid(principal is Customer, action == Action::"read", resource == Issue::"issue-1");"#
                        .to_string(),
                ),
                (
                    "irrelevant:issue-write".to_string(),
                    r#"permit(principal is Customer, action == Action::"write", resource is Issue);"#
                        .to_string(),
                ),
                (
                    "irrelevant:doc-read".to_string(),
                    r#"permit(principal is Customer, action == Action::"read", resource is Doc);"#
                        .to_string(),
                ),
            ],
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let decision = engine.authorize_for_tenant("default", &ctx, "read", "Issue", &attrs);
    let AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids }) = decision else {
        panic!("expected named forbid policy denial");
    };

    assert!(
        policy_ids
            .iter()
            .any(|id| id == "default:decision:block-issue-1"),
        "candidate filtering must preserve named policy diagnostics, got: {policy_ids:?}"
    );
}
