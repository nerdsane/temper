use super::*;
use crate::context::SecurityContext;
use crate::error::AuthzDenial;

const PM_ISSUE_POLICY: &str =
    include_str!("../../../../os-apps/project-management/specs/policies/issue.cedar");

fn admin_context() -> SecurityContext {
    SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "admin-1".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
    ])
}

fn customer_context(id: &str) -> SecurityContext {
    SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), id.to_string()),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
    ])
}

#[test]
fn test_permissive_engine_allows_all() {
    // Permissive engine has a catch-all permit policy.
    let engine = AuthzEngine::permissive();
    let ctx = customer_context("cust-1");
    let attrs = HashMap::new();

    let decision = engine.authorize(&ctx, "read", "Order", &attrs);
    assert_eq!(decision, AuthzDecision::Allow);
}

#[test]
fn test_system_bypass() {
    let engine = AuthzEngine::permissive();
    let ctx = SecurityContext::system();
    let attrs = HashMap::new();

    let decision = engine.authorize_or_bypass(&ctx, "read", "Order", &attrs);
    assert!(decision.is_allowed());
}

#[test]
fn test_admin_permit_policy() {
    let policy = r#"
        permit(
            principal is Admin,
            action,
            resource
        );
    "#;

    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = admin_context();
    let attrs = HashMap::new();

    let decision = engine.authorize(&ctx, "read", "Order", &attrs);
    assert!(
        decision.is_allowed(),
        "admin should be allowed, got: {decision:?}"
    );
}

#[test]
fn test_customer_denied_without_matching_policy() {
    let policy = r#"
        permit(
            principal is Admin,
            action,
            resource
        );
    "#;

    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = customer_context("cust-1");
    let attrs = HashMap::new();

    let decision = engine.authorize(&ctx, "read", "Order", &attrs);
    assert!(!decision.is_allowed(), "customer should be denied");
}

#[test]
fn test_invalid_policy_returns_error() {
    let result = AuthzEngine::new("this is not valid cedar");
    assert!(result.is_err());
}

#[test]
fn test_decision_is_allowed() {
    assert!(AuthzDecision::Allow.is_allowed());
    assert!(!AuthzDecision::Deny(AuthzDenial::NoMatchingPermit).is_allowed());
}

#[test]
fn test_hot_reload_replaces_policies() {
    // Start with admin-only policy
    let admin_policy = r#"
        permit(
            principal is Admin,
            action,
            resource
        );
    "#;
    let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");
    assert_eq!(engine.policy_count(), 1);

    // Customer is denied
    let ctx = customer_context("cust-1");
    let attrs = HashMap::new();
    assert!(!engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());

    // Hot-reload to customer-permitting policy
    let customer_policy = r#"
        permit(
            principal is Customer,
            action,
            resource
        );
    "#;
    engine
        .reload_policies(customer_policy)
        .expect("reload should succeed");
    assert_eq!(engine.policy_count(), 1);

    // Now customer is allowed
    assert!(engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());

    // Admin is now denied (only customer policy active)
    let admin_ctx = admin_context();
    assert!(
        !engine
            .authorize(&admin_ctx, "read", "Order", &attrs)
            .is_allowed()
    );
}

#[test]
fn test_hot_reload_invalid_preserves_existing() {
    let admin_policy = r#"
        permit(
            principal is Admin,
            action,
            resource
        );
    "#;
    let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");

    // Try to reload with invalid policy — should fail
    let result = engine.reload_policies("not valid cedar at all");
    assert!(result.is_err());

    // Original policy still works
    let ctx = admin_context();
    let attrs = HashMap::new();
    assert!(engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());
    assert_eq!(engine.policy_count(), 1);
}

#[test]
fn test_hot_reload_to_empty() {
    let admin_policy = r#"
        permit(
            principal is Admin,
            action,
            resource
        );
    "#;
    let engine = AuthzEngine::new(admin_policy).expect("initial policy should parse");

    // Reload with empty policy set
    engine
        .reload_policies("")
        .expect("empty policy should parse");
    assert_eq!(engine.policy_count(), 0);

    // Admin is now denied (no policies)
    let ctx = admin_context();
    let attrs = HashMap::new();
    assert!(!engine.authorize(&ctx, "read", "Order", &attrs).is_allowed());
}

#[test]
fn test_agent_type_in_cedar_context() {
    let engine = AuthzEngine::permissive();
    engine
        .reload_policies(
            "permit(principal is Agent, action == Action::\"read\", resource is Doc) when { context.agentType == \"claude-code\" };",
        )
        .unwrap();
    // With matching agentType -> Allow
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-1".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ])
    .with_agent_context(Some("bot-1"), None, Some("claude-code"));
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("doc-1"));
    let result = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(result.is_allowed(), "should allow claude-code agent");

    // Without matching agentType -> Deny
    let ctx2 = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-2".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ])
    .with_agent_context(Some("bot-2"), None, Some("openclaw"));
    let mut attrs2 = HashMap::new();
    attrs2.insert("id".to_string(), serde_json::json!("doc-2"));
    let result2 = engine.authorize(&ctx2, "read", "Doc", &attrs2);
    assert!(!result2.is_allowed(), "should deny non-claude-code agent");
}

#[test]
fn test_exact_agent_principal_match() {
    // Approval policies use exact UID match: `principal == Agent::"bot-1"`
    // This requires the principal entity to exist in the entity store.
    let policy =
        r#"permit(principal == Agent::"bot-1", action == Action::"Assign", resource is Issue);"#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-1".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));
    let decision = engine.authorize(&ctx, "Assign", "Issue", &attrs);
    assert!(
        decision.is_allowed(),
        "exact principal match should work: {decision:?}"
    );
}

#[test]
fn test_exact_principal_match_wrong_id_denied() {
    let policy =
        r#"permit(principal == Agent::"bot-1", action == Action::"Assign", resource is Issue);"#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-2".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));
    let decision = engine.authorize(&ctx, "Assign", "Issue", &attrs);
    assert!(
        !decision.is_allowed(),
        "wrong principal ID should be denied"
    );
}

#[test]
fn test_principal_attribute_access_in_policy() {
    // PM base policies use: `principal.agent_type in ["supervisor", "human"]`
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"Triage",
            resource is Issue
        ) when {
            principal.agent_type == "supervisor"
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();

    // With matching agent_type → Allow
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-1".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "supervisor".to_string()),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));
    let decision = engine.authorize(&ctx, "Triage", "Issue", &attrs);
    assert!(
        decision.is_allowed(),
        "supervisor agent_type should be allowed: {decision:?}"
    );

    // Without matching agent_type → Deny
    let ctx2 = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "bot-2".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "worker".to_string()),
    ]);
    let decision2 = engine.authorize(&ctx2, "Triage", "Issue", &attrs);
    assert!(
        !decision2.is_allowed(),
        "non-supervisor agent_type should be denied"
    );
}

#[test]
fn test_principal_agent_type_set_membership_filtering() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"Assign",
            resource is Issue
        ) when {
            ["supervisor", "human"].contains(principal.agent_type)
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();

    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let supervisor_ctx = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "bot-supervisor".to_string(),
        ),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "supervisor".to_string()),
    ]);
    let supervisor_decision = engine.authorize(&supervisor_ctx, "Assign", "Issue", &attrs);
    assert!(
        supervisor_decision.is_allowed(),
        "set membership should allow supervisor agent_type: {supervisor_decision:?}"
    );

    let worker_ctx = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "bot-worker".to_string(),
        ),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "worker".to_string()),
    ]);
    let worker_decision = engine.authorize(&worker_ctx, "Assign", "Issue", &attrs);
    assert!(
        !worker_decision.is_allowed(),
        "set membership should deny non-listed agent_type"
    );
}

#[test]
fn test_context_entity_status_in_cedar_context() {
    // Policy that gates on context.ctx_parent_agent_status
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"canary_deploy",
            resource is DeployWorkflow
        ) when {
            context.ctx_parent_agent_status == "canary_ok"
        };
    "#;

    let engine = AuthzEngine::new(policy).unwrap();

    let ctx = SecurityContext::from_headers(&[
        ("x-temper-principal-id".to_string(), "agent-1".to_string()),
        ("x-temper-principal-kind".to_string(), "agent".to_string()),
    ]);

    // Without context entity status: should deny
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("deploy-1"));
    let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
    assert!(
        !decision.is_allowed(),
        "should deny without context entity status"
    );

    // With context entity status matching: should allow
    attrs.insert(
        "ctx_parent_agent_status".to_string(),
        serde_json::json!("canary_ok"),
    );
    let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
    assert!(
        decision.is_allowed(),
        "should allow with matching context entity status, got: {decision:?}"
    );

    // With wrong context entity status: should deny
    attrs.insert(
        "ctx_parent_agent_status".to_string(),
        serde_json::json!("planning"),
    );
    let decision = engine.authorize(&ctx, "canary_deploy", "DeployWorkflow", &attrs);
    assert!(
        !decision.is_allowed(),
        "should deny with wrong context entity status"
    );
}

#[test]
fn test_pm_assign_denies_openclaw_agent_type() {
    let engine = AuthzEngine::new(PM_ISSUE_POLICY).unwrap();

    // Even with verified identity, openclaw is not in ["supervisor", "human"].
    let ctx = SecurityContext::from_resolved_identity("bot-openclaw", "openclaw", None);

    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let decision = engine.authorize(&ctx, "Assign", "Issue", &attrs);
    assert!(
        !decision.is_allowed(),
        "openclaw agent_type must be denied for Assign: {decision:?}"
    );
}

#[test]
fn test_pm_assign_allows_supervisor_agent_type() {
    let engine = AuthzEngine::new(PM_ISSUE_POLICY).unwrap();

    // Use credential-resolved identity (ADR-0033) — agentTypeVerified is true.
    let ctx = SecurityContext::from_resolved_identity("bot-supervisor", "supervisor", None);

    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let decision = engine.authorize(&ctx, "Assign", "Issue", &attrs);
    assert!(
        decision.is_allowed(),
        "verified supervisor agent_type must be allowed for Assign: {decision:?}"
    );
}

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
