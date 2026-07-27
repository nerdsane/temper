use super::*;

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
