use super::*;
use crate::context::SecurityContext;
use crate::error::AuthzDenial;

mod attributes;
mod context_isolation;
mod tenant;

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
fn quoted_action_id_is_constructed_without_string_parsing() {
    let engine = AuthzEngine::permissive();
    let ctx = customer_context("cust-1");
    let attrs = HashMap::new();

    let decision = engine.authorize(&ctx, "bad\"action", "Order", &attrs);
    assert!(
        decision.is_allowed(),
        "quotes are valid inside a typed Cedar entity id: {decision:?}"
    );
}

#[test]
fn quoted_principal_and_resource_ids_are_typed_not_reparsed() {
    let engine = AuthzEngine::permissive();
    let ctx = customer_context("cust\"\\id");
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("order\"\\id"))]);

    let decision = engine.authorize(&ctx, "read", "Order", &attrs);
    assert!(
        decision.is_allowed(),
        "typed Cedar ids must preserve quotes and backslashes: {decision:?}"
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
