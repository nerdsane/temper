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
    assert!(decision.is_allowed());
}

#[test]
fn test_system_bypass() {
    let engine = AuthzEngine::permissive();
    let ctx = SecurityContext::system();
    let attrs = HashMap::new();

    let decision = engine.authorize_or_bypass(&ctx, "read", "Order", &attrs);
    assert!(decision.is_allowed());
}

/// ADR-0046 regression: the `is_system → Allow` short-circuit is gone.
/// System principals must be authorized by an explicit Cedar policy.
/// [`AuthzEngine::empty`] now installs the built-in `system-platform`
/// broad-permit, so System is still Allow (migration preserves behavior),
/// but goes through Cedar evaluation — no more silent bypass.
#[test]
fn system_authorized_via_system_platform_policy_not_bypass() {
    let engine = AuthzEngine::empty();
    let attrs = HashMap::new();

    // System is Allow via system-platform policy.
    let sys = SecurityContext::system();
    let decision = engine.authorize(&sys, "AnyAction", "AnyResource", &attrs);
    assert!(
        decision.is_allowed(),
        "System must be authorized via system-platform policy, got: {decision:?}"
    );

    // Non-system principal against empty engine is denied (Cedar default-deny).
    // This would have been silently bypassed if we still used is_system.
    let customer = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "alice".to_string()),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
    ]);
    let decision = engine.authorize(&customer, "AnyAction", "AnyResource", &attrs);
    assert!(
        !decision.is_allowed(),
        "Customer should hit Cedar default-deny with no user policy. Got: {decision:?}"
    );
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
fn test_scoped_customer_principal_exposes_account_id() {
    let policy = r#"
        permit(
            principal is Customer,
            action == Action::"Update",
            resource is Ref
        ) when {
            principal.scopes.contains("repo:write") &&
            context.repositoryOwnerAccountId == principal.accountId
        };
    "#;

    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "acct-1".to_string()),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
        (
            "X-Temper-Principal-Scopes".to_string(),
            "repo:read,repo:write".to_string(),
        ),
    ]);
    let attrs = HashMap::from([
        (
            "Id".to_string(),
            serde_json::Value::String("rf-rp-acct-repo-refs-heads-main".to_string()),
        ),
        (
            "repositoryOwnerAccountId".to_string(),
            serde_json::Value::String("acct-1".to_string()),
        ),
    ]);

    let decision = engine.authorize(&ctx, "Update", "Ref", &attrs);
    assert!(
        decision.is_allowed(),
        "scoped owner should be allowed, got {decision:?}"
    );
}

#[test]
fn test_invalid_policy_returns_error() {
    let result = AuthzEngine::new("this is not valid cedar");
    assert!(result.is_err());
}

#[test]
fn invalid_action_denies_through_instrumented_error_path() {
    let engine = AuthzEngine::permissive();
    let ctx = customer_context("cust-1");
    let attrs = HashMap::new();

    let decision = engine.authorize(&ctx, "bad\"action", "Order", &attrs);
    assert!(
        matches!(decision, AuthzDecision::Deny(AuthzDenial::InvalidAction(_))),
        "invalid action should deny with typed reason, got: {decision:?}"
    );
}

#[test]
fn test_decision_is_allowed() {
    assert!((AuthzDecision::Allow { policy_ids: vec![] }).is_allowed());
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
fn test_resource_attribute_access_in_policy() {
    // Temper app policies gate claimed work on entity state, e.g.
    // `principal.id == resource.worker_id`.
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"StartLocal",
            resource is WorkerRun
        ) when {
            principal.agent_type == "worker" &&
            resource has worker_id &&
            principal.id == resource.worker_id
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();

    let worker_ctx = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "local-codex-worker".to_string(),
        ),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "worker".to_string()),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("wr-1"));
    attrs.insert(
        "worker_id".to_string(),
        serde_json::json!("local-codex-worker"),
    );

    let decision = engine.authorize(&worker_ctx, "StartLocal", "WorkerRun", &attrs);
    assert!(
        decision.is_allowed(),
        "claimed worker should be allowed through resource.worker_id: {decision:?}"
    );

    let other_ctx = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "other-worker".to_string(),
        ),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Type".to_string(), "worker".to_string()),
    ]);
    let decision = engine.authorize(&other_ctx, "StartLocal", "WorkerRun", &attrs);
    assert!(
        !decision.is_allowed(),
        "different worker must not satisfy resource.worker_id"
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
        // Should name the source, e.g. "decision:abc#1", never "policy0".
        let has_meaningful = policy_ids
            .iter()
            .any(|id| id.starts_with("os-app:pm#") || id.starts_with("decision:abc#"));
        assert!(
            has_meaningful,
            "policy IDs should be meaningful, got: {policy_ids:?}"
        );
    }
    // NoMatchingPermit is also acceptable since user-1 != bot-1
}

#[test]
fn denial_names_the_cedar_file_a_statement_came_from() {
    // ARN-286: the rendered denial must point a human at a file and at the
    // statement inside it, not at a positional id like "policy1874".
    let engine = AuthzEngine::empty();

    engine
        .reload_tenant_policies_named(
            "katagami",
            &[(
                "katagami-commons/art_style.cedar".to_string(),
                concat!(
                    "permit(principal is Customer, action == Action::\"read\", resource is ArtStyle);\n",
                    "forbid(principal is Customer, action == Action::\"update\", resource is ArtStyle);\n",
                )
                .to_string(),
            )],
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("as-1"));

    let decision = engine.authorize_for_tenant("katagami", &ctx, "update", "ArtStyle", &attrs);
    let AuthzDecision::Deny(denial) = decision else {
        panic!("forbid statement should deny the update");
    };

    assert_eq!(
        denial.to_string(),
        "denied by policy: katagami-commons/art_style.cedar#2",
        "the denial must name the source file and the statement within it"
    );
}

#[test]
fn named_policy_ids_are_stable_when_a_sibling_statement_is_added() {
    // Every statement carries a "#n" suffix, including the only statement in a
    // one-statement file, so adding a second statement later does not silently
    // change what an already-reported id refers to.
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies_named(
            "t",
            &[(
                "app/only.cedar".to_string(),
                r#"forbid(principal is Customer, action == Action::"read", resource is Doc);"#
                    .to_string(),
            )],
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("d-1"));

    let decision = engine.authorize_for_tenant("t", &ctx, "read", "Doc", &attrs);
    let AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids }) = decision else {
        panic!("expected a policy denial");
    };
    assert_eq!(policy_ids, vec!["app/only.cedar#1".to_string()]);
}

#[test]
fn adding_one_policy_keeps_the_labels_of_the_others() {
    // Approving a governance decision adds a policy. It must not flatten the
    // tenant back to positional ids for everything else (ARN-286).
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies_named(
            "katagami",
            &[(
                "katagami-commons/art_style.cedar".to_string(),
                concat!(
                    "permit(principal is Customer, action == Action::\"read\", resource is ArtStyle);\n",
                    "forbid(principal is Customer, action == Action::\"update\", resource is ArtStyle);\n",
                )
                .to_string(),
            )],
        )
        .unwrap();

    engine
        .upsert_tenant_policy_source(
            "katagami",
            "katagami/generated/GD-1.cedar",
            r#"permit(principal is Customer, action == Action::"list", resource is ArtStyle);"#,
        )
        .unwrap();

    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("as-1"));

    // The generated policy took effect...
    assert!(
        engine
            .authorize_for_tenant("katagami", &ctx, "list", "ArtStyle", &attrs)
            .is_allowed()
    );

    // ...and the pre-existing file still names itself in a denial.
    let decision = engine.authorize_for_tenant("katagami", &ctx, "update", "ArtStyle", &attrs);
    let AuthzDecision::Deny(denial) = decision else {
        panic!("forbid statement should still deny the update");
    };
    assert_eq!(
        denial.to_string(),
        "denied by policy: katagami-commons/art_style.cedar#2"
    );
}

#[test]
fn adding_a_policy_to_a_blob_loaded_tenant_labels_the_blob_once() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies(
            "blobby",
            r#"forbid(principal is Customer, action == Action::"read", resource is Doc);"#,
        )
        .unwrap();

    engine
        .upsert_tenant_policy_source(
            "blobby",
            "blobby/generated/GD-1.cedar",
            r#"permit(principal is Customer, action == Action::"list", resource is Doc);"#,
        )
        .unwrap();

    let labels: Vec<String> = engine
        .tenant_policy_sources("blobby")
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(
        labels,
        vec![
            unlabelled_source_label("blobby"),
            "blobby/generated/GD-1.cedar".to_string()
        ]
    );

    // The blob's statements still apply, now under a label that says where
    // they came from.
    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("d-1"));
    let decision = engine.authorize_for_tenant("blobby", &ctx, "read", "Doc", &attrs);
    let AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids }) = decision else {
        panic!("expected the blob's forbid to still deny");
    };
    assert_eq!(
        policy_ids,
        vec![format!("{}#1", unlabelled_source_label("blobby"))]
    );
}

#[test]
fn labelled_sources_are_readable_back_for_rebuilds() {
    let engine = AuthzEngine::empty();
    let sources = vec![
        (
            "katagami-commons/art_style.cedar".to_string(),
            r#"permit(principal is Customer, action == Action::"read", resource is ArtStyle);"#
                .to_string(),
        ),
        (
            "katagami-curation/review.cedar".to_string(),
            r#"permit(principal is Customer, action == Action::"read", resource is Review);"#
                .to_string(),
        ),
    ];
    engine
        .reload_tenant_policies_named("katagami", &sources)
        .unwrap();

    assert_eq!(engine.tenant_policy_sources("katagami"), sources);

    // An unlabelled blob reload reports no sources rather than inventing one.
    engine
        .reload_tenant_policies("blobby", "permit(principal, action, resource);")
        .unwrap();
    assert!(engine.tenant_policy_sources("blobby").is_empty());
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
        policy_ids.iter().any(|id| id == "decision:block-issue-1#1"),
        "candidate filtering must preserve named policy diagnostics, got: {policy_ids:?}"
    );
}

/// Two policy applications that overlap in time must both survive.
///
/// The regression: `upsert_tenant_policy_source` used to read the tenant's
/// sources under a read lock, mutate a local `Vec`, then reload under a write
/// lock. A reads, B reads, A writes, B writes — A's policy is gone and both
/// calls returned `Ok`. Here the first writer is parked *inside* the
/// critical section while the second runs start to finish, so a non-atomic
/// sequence loses one of them every run rather than occasionally.
#[test]
fn concurrent_policy_upserts_do_not_lose_each_other() {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    let engine = Arc::new(AuthzEngine::empty());
    engine
        .reload_tenant_policies_named(
            "katagami",
            &[(
                "katagami-commons/art_style.cedar".to_string(),
                r#"permit(principal is Customer, action == Action::"read", resource is ArtStyle);"#
                    .to_string(),
            )],
        )
        .unwrap();

    let (entered_tx, entered_rx) = mpsc::channel();
    let slow_writer = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            engine
                .update_tenant_policy_sources("katagami", move |mut sources| {
                    entered_tx.send(()).unwrap();
                    // Hold the critical section open long enough for the other
                    // writer to complete if nothing is serializing them.
                    std::thread::sleep(Duration::from_millis(300));
                    sources.push((
                        "katagami/generated/GD-slow.cedar".to_string(),
                        r#"permit(principal is Customer, action == Action::"list", resource is ArtStyle);"#
                            .to_string(),
                    ));
                    sources
                })
                .unwrap();
        })
    };

    entered_rx.recv().unwrap();
    engine
        .upsert_tenant_policy_source(
            "katagami",
            "katagami/generated/GD-fast.cedar",
            r#"permit(principal is Customer, action == Action::"create", resource is ArtStyle);"#,
        )
        .unwrap();
    slow_writer.join().unwrap();

    let labels: Vec<String> = engine
        .tenant_policy_sources("katagami")
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(
        labels.contains(&"katagami-commons/art_style.cedar".to_string()),
        "the pre-existing app policy must survive both upserts, got: {labels:?}"
    );
    assert!(
        labels.contains(&"katagami/generated/GD-slow.cedar".to_string()),
        "the parked writer's policy must survive, got: {labels:?}"
    );
    assert!(
        labels.contains(&"katagami/generated/GD-fast.cedar".to_string()),
        "the concurrent writer's policy must survive, got: {labels:?}"
    );

    // Both generated policies are actually in effect, not merely recorded.
    let ctx = customer_context("user-1");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("as-1"));
    for action in ["read", "list", "create"] {
        assert!(
            engine
                .authorize_for_tenant("katagami", &ctx, action, "ArtStyle", &attrs)
                .is_allowed(),
            "action '{action}' should be permitted after both upserts"
        );
    }
}

/// The same guarantee under real contention: every writer's policy is present
/// once all of them have returned.
#[test]
fn every_racing_policy_upsert_is_retained() {
    use std::sync::{Arc, Barrier};

    const WRITERS: usize = 8;

    let engine = Arc::new(AuthzEngine::empty());
    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers: Vec<_> = (0..WRITERS)
        .map(|i| {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                engine
                    .upsert_tenant_policy_source(
                        "katagami",
                        &format!("katagami/generated/GD-{i}.cedar"),
                        &format!(
                            r#"permit(principal is Customer, action == Action::"read", resource == ArtStyle::"as-{i}");"#
                        ),
                    )
                    .unwrap();
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }

    let labels: Vec<String> = engine
        .tenant_policy_sources("katagami")
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    for i in 0..WRITERS {
        assert!(
            labels.contains(&format!("katagami/generated/GD-{i}.cedar")),
            "writer {i}'s policy was lost, got: {labels:?}"
        );
    }
}
