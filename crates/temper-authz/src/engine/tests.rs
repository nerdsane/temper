use super::*;
use crate::context::SecurityContext;
use crate::error::AuthzDenial;

const PM_ISSUE_POLICY: &str =
    include_str!("../../../../os-apps/project-management/specs/policies/issue.cedar");

fn admin_context() -> SecurityContext {
    SecurityContext {
        principal: crate::context::Principal {
            id: "admin-1".to_string(),
            kind: crate::context::PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "test-admin".to_string(),
    }
}

fn customer_context(id: &str) -> SecurityContext {
    SecurityContext {
        principal: crate::context::Principal {
            id: id.to_string(),
            kind: crate::context::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "test-customer".to_string(),
    }
}

fn agent_context(id: &str, agent_type: &str) -> SecurityContext {
    SecurityContext::from_resolved_identity(id, agent_type, None)
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
    let customer = customer_context("alice");
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
    let mut ctx = customer_context("acct-1");
    ctx.principal.attributes.insert(
        "scopes".to_string(),
        serde_json::json!(["repo:read", "repo:write"]),
    );
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
    let ctx = agent_context("bot-1", "claude-code");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("doc-1"));
    let result = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(result.is_allowed(), "should allow claude-code agent");

    // Without matching agentType -> Deny
    let ctx2 = agent_context("bot-2", "openclaw");
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
    let ctx = agent_context("bot-1", "test-agent");
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
    let ctx = agent_context("bot-2", "test-agent");
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
    let ctx = agent_context("bot-1", "supervisor");
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("issue-1"));
    let decision = engine.authorize(&ctx, "Triage", "Issue", &attrs);
    assert!(
        decision.is_allowed(),
        "supervisor agent_type should be allowed: {decision:?}"
    );

    // Without matching agent_type → Deny
    let ctx2 = agent_context("bot-2", "worker");
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

    let worker_ctx = agent_context("local-codex-worker", "worker");
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

    let other_ctx = agent_context("other-worker", "worker");
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

    let supervisor_ctx = agent_context("bot-supervisor", "supervisor");
    let supervisor_decision = engine.authorize(&supervisor_ctx, "Assign", "Issue", &attrs);
    assert!(
        supervisor_decision.is_allowed(),
        "set membership should allow supervisor agent_type: {supervisor_decision:?}"
    );

    let worker_ctx = agent_context("bot-worker", "worker");
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

    let ctx = agent_context("agent-1", "test-agent");

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
fn missing_tenant_never_inherits_permissive_fallback() {
    let engine = AuthzEngine::permissive();
    let ctx = customer_context("anonymous");
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("doc-1"))]);

    assert!(
        engine.authorize(&ctx, "read", "Doc", &attrs).is_allowed(),
        "negative control: the explicit global compatibility policy is permissive"
    );
    let decision = engine.authorize_for_tenant("unloaded", &ctx, "read", "Doc", &attrs);
    assert_eq!(
        decision,
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
        "tenant-scoped authorization must fail closed when its policy is absent"
    );
}

#[test]
fn missing_tenant_system_uses_only_the_builtin_platform_policy() {
    let engine = AuthzEngine::permissive();
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("doc-1"))]);

    assert!(
        engine
            .authorize_for_tenant(
                "unloaded",
                &SecurityContext::system(),
                "bootstrap",
                "Doc",
                &attrs,
            )
            .is_allowed(),
        "kernel System work must remain authorized by the explicit built-in policy"
    );
    assert_eq!(
        engine.authorize_for_tenant(
            "unloaded",
            &customer_context("anonymous"),
            "read",
            "Doc",
            &attrs,
        ),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
        "an unloaded tenant must not inherit the permissive compatibility policy"
    );
}

#[test]
fn failed_tenant_reload_preserves_last_known_good_without_global_fallback() {
    let engine = AuthzEngine::permissive();
    engine
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal is Customer, action == Action::"read", resource is Doc);"#,
        )
        .unwrap();
    assert!(
        engine
            .reload_tenant_policies("tenant-a", "not cedar")
            .is_err()
    );

    let ctx = customer_context("customer-1");
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("doc-1"))]);
    assert!(
        engine
            .authorize_for_tenant("tenant-a", &ctx, "read", "Doc", &attrs)
            .is_allowed(),
        "a failed reload must preserve the exact tenant's last-known-good policy"
    );
    assert!(
        !engine
            .authorize_for_tenant("tenant-a", &ctx, "write", "Doc", &attrs)
            .is_allowed(),
        "a failed reload must not widen to the permissive global policy"
    );
    assert!(
        !engine
            .authorize_for_tenant("tenant-b", &ctx, "read", "Doc", &attrs)
            .is_allowed(),
        "another unloaded tenant must remain denied"
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

/// Resource attributes must not satisfy a session-scoped permit. The only
/// legitimate `context.sessionId` is the grant-checked value on SecurityContext.
#[test]
fn resource_session_id_cannot_satisfy_session_scoped_permit() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"create",
            resource is Order
        ) when { context.sessionId == "approved-session" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "operator", None);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    attrs.insert(
        "sessionId".to_string(),
        serde_json::json!("approved-session"),
    );

    let decision = engine.authorize(&ctx, "create", "Order", &attrs);
    assert!(
        !decision.is_allowed(),
        "caller-supplied resource.sessionId must not satisfy context.sessionId, got: {decision:?}"
    );
}

/// The grant-checked session id on SecurityContext still authorizes.
#[test]
fn grant_checked_session_id_satisfies_session_scoped_permit() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"create",
            resource is Order
        ) when { context.sessionId == "approved-session" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx =
        SecurityContext::from_resolved_identity("agent-1", "operator", Some("approved-session"));
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));

    let decision = engine.authorize(&ctx, "create", "Order", &attrs);
    assert!(
        decision.is_allowed(),
        "grant-checked context.sessionId must still match, got: {decision:?}"
    );
}

/// A resource body must not overwrite a different grant-checked session id.
#[test]
fn resource_session_id_cannot_overwrite_grant_checked_session_id() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"create",
            resource is Order
        ) when { context.sessionId == "forged-session" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "operator", Some("real-session"));
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    attrs.insert("sessionId".to_string(), serde_json::json!("forged-session"));

    let decision = engine.authorize(&ctx, "create", "Order", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource.sessionId must not overwrite the grant-checked session, got: {decision:?}"
    );
}

/// Resource attrs must not mint `agentType` / `agentTypeVerified` that
/// `from_resolved_identity` is supposed to be the only source of.
#[test]
fn resource_attrs_cannot_forge_agent_type_verified() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"create",
            resource is Order
        ) when { context.agentType == "supervisor" && context.agentTypeVerified == true };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "operator", None);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    attrs.insert("agentType".to_string(), serde_json::json!("supervisor"));
    attrs.insert("agentTypeVerified".to_string(), serde_json::json!(true));

    let decision = engine.authorize(&ctx, "create", "Order", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource agentType/agentTypeVerified must not overwrite Cedar context, got: {decision:?}"
    );
}

/// Resource `role` must not become `context.role`.
#[test]
fn resource_role_cannot_satisfy_role_scoped_permit() {
    let policy = r#"
        permit(
            principal is Agent,
            action == Action::"create",
            resource is Order
        ) when { context.role == "admin" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "operator", None);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    attrs.insert("role".to_string(), serde_json::json!("admin"));

    let decision = engine.authorize(&ctx, "create", "Order", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource.role must not satisfy context.role, got: {decision:?}"
    );
}
