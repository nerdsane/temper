//! Focused state-timeout regression group.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_timeout_fires_and_transitions_entity() {
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
    );
    let system = ActorSystem::new("state-timeout-integration");
    let state = ServerState::from_registry(system, registry);

    let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
    let agent_ctx = AgentContext::for_service("timeout-scheduler");

    // Create the entity so it lands in `Open`.
    let created = state
        .get_or_create_tenant_entity(&tenant, "Ticket", "t-1", serde_json::json!({}))
        .await
        .expect("create ticket");
    assert_eq!(created.state.status, "Open");

    // Arm the state_timeout by dispatching a no-op Action? We need to
    // trigger `arm_state_timeouts_if_needed`. Creation itself doesn't
    // go through dispatch, so arm via a self-loop transition — easiest
    // path is to dispatch a direct RecordProgress-like action. Ticket
    // spec has no self-loop, so we simulate by inspecting initial state
    // and letting the watchdog fire by entering Open via Configure. For
    // this test, we force an arm by directly calling the ServerState
    // hook with a synthesized PostDispatchContext.
    let response = state
        .get_tenant_entity_state(&tenant, "Ticket", "t-1")
        .await
        .unwrap();
    let ctx = PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id: "t-1",
        action: "__Created",
        agent_ctx: &agent_ctx,
        dispatch_idempotency_key: None,
        action_params: &serde_json::json!({}),
        await_integration: false,
        actor_uid: None,
    };
    state.arm_state_timeouts_if_needed(&ctx, &response);

    // Timer is 1s; give it 2s to fire + dispatch + apply.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "t-1")
        .await
        .unwrap();
    assert_eq!(
        after.state.status, "InProgress",
        "state_timeout should have fired AssignAgent and moved Ticket to InProgress"
    );
    // Sanity: dispatch the same action explicitly — must fail because
    // AssignAgent is no longer valid from InProgress. This confirms the
    // transition actually went through the state machine (not a faked
    // status update).
    let retry = state
        .dispatch(DispatchCommand {
            tenant: &tenant,
            entity_type: "Ticket",
            entity_id: "t-1",
            action: "AssignAgent",
            params: serde_json::json!({}),
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await;
    if let Ok(r) = retry {
        assert!(
            !r.success,
            "AssignAgent must be rejected from InProgress (state machine integrity check)"
        );
    }
}
