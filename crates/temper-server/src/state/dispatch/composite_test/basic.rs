//! Focused composite-dispatch regression group.

use super::*;

#[tokio::test]
async fn composite_action_rejects_caller_supplied_sub_writes() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let err = state
        .dispatch_tenant_action(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            json!({
                "Reason": "unit-test",
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-1",
                    "action": "Create",
                    "params": { "Name": "created through composite" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("caller-supplied sub_writes should be rejected");

    assert!(
        err.contains("cannot accept caller-supplied sub_writes"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn composite_integration_result_executes_declared_sub_writes() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            json!({ "Reason": "unit-test" }),
            &agent,
        )
        .await
        .expect("composite parent action should run");

    assert!(response.success);
    assert_eq!(response.state.status, "Active");
    assert!(response.state.fields.get("sub_writes").is_none());

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-1",
                    "action": "Create",
                    "params": { "Name": "created through composite integration" }
                }]
            }),
            &agent,
        )
        .await
        .expect("composite integration result should apply");

    assert!(applied);

    let child = state
        .get_tenant_entity_state(&tenant, "Child", "child-1")
        .await
        .expect("child state should be readable");
    assert_eq!(child.state.status, "Active");
    assert_eq!(
        child.state.fields.get("Name"),
        Some(&json!("created through composite integration"))
    );
}

#[tokio::test]
async fn composite_sub_write_authorization_receives_action_context() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild"
                };
                "#,
        )
        .expect("policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-auth-ok",
                    "action": "Create",
                    "params": { "Name": "authorized through action_context" }
                }]
            }),
            &agent,
        )
        .await
        .expect("composite sub-write should be authorized by action_context");
    assert!(applied);

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Other.Action"
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-auth-denied",
                    "action": "Create",
                    "params": { "Name": "should be denied" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("mismatched action_context should deny sub-write")
        .to_string();
    assert!(
        err.contains("sub-write 0 denied"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn composite_ref_sub_write_uses_parent_gate_for_declared_ref_updates() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_tenant_policies_named(
            tenant.as_str(),
            &[(
                "unrelated-child-create".to_string(),
                r#"
                    permit(
                      principal is Agent,
                      action == Action::"Create",
                      resource is Child
                    );
                    "#
                .to_string(),
            )],
        )
        .expect("unrelated tenant policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-auth",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Ref",
                    "entity_id": "rf-auth-main",
                    "action": "Create",
                    "params": {
                        "RepositoryId": "repo-auth",
                        "Name": "refs/heads/main",
                        "TargetCommitSha": "1111111111111111111111111111111111111111",
                        "Kind": "branch",
                        "PreviousCommitSha": "0000000000000000000000000000000000000000"
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("declared ref_updates sub-write should use the parent composite gate");

    assert!(applied);
    let reference = state
        .get_tenant_entity_state(&tenant, "Ref", "rf-auth-main")
        .await
        .expect("ref state should be readable");
    assert_eq!(reference.state.status, "Active");
    assert_eq!(
        reference.state.fields.get("TargetCommitSha"),
        Some(&json!("1111111111111111111111111111111111111111"))
    );
}

#[tokio::test]
async fn composite_app_create_sub_write_authorization_can_enforce_owner_scope() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext {
        security_ctx: Some(SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), "alice".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ])),
        ..Default::default()
    };

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal,
                  action == Action::"Create",
                  resource is App
                );

                forbid(
                  principal,
                  action == Action::"Create",
                  resource is App
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  !(resource.OwnerId == principal.accountId ||
                    (principal has scopes &&
                     principal.scopes.contains("admin:repos")))
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-owner-scope",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-bob-owned",
                    "action": "Create",
                    "params": { "OwnerId": "bob", "Name": "bob-app" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("caller must not create a composite App row under another owner")
        .to_string();
    assert!(
        err.contains("sub-write 0 denied"),
        "unexpected error: {err}"
    );
    assert!(!state.entity_exists(&tenant, "App", "app-bob-owned"));

    let allowed = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-owner-scope",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-owned",
                    "action": "Create",
                    "params": { "OwnerId": "alice", "Name": "alice-app" }
                }]
            }),
            &agent,
        )
        .await
        .expect("caller should create a composite App row under their own owner");
    assert!(allowed);
    assert!(state.entity_exists(&tenant, "App", "app-alice-owned"));
}
