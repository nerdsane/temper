use super::*;
pub(super) async fn directed_evolution_create(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) {
    state
        .server
        .get_or_create_tenant_entity(tenant, entity_type, entity_id, serde_json::json!({}))
        .await
        .unwrap_or_else(|error| panic!("create {entity_type}/{entity_id}: {error}"));
}

pub(super) async fn directed_evolution_dispatch(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
    await_integration: bool,
) -> temper_server::EntityResponse {
    use temper_server::state::DispatchCommand;

    let agent_ctx = AgentContext::system();
    let response = state
        .server
        .dispatch(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx: &agent_ctx,
            await_integration,
            await_reactions: true,
        })
        .await
        .unwrap_or_else(|error| {
            panic!("dispatch {entity_type}/{entity_id}.{action} failed: {error}")
        });
    assert!(
        response.success,
        "dispatch {entity_type}/{entity_id}.{action} returned error: {:?}",
        response.error
    );
    response
}

pub(super) async fn directed_evolution_entity(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> temper_server::EntityResponse {
    state
        .server
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .unwrap_or_else(|error| panic!("load {entity_type}/{entity_id}: {error}"))
}

pub(super) async fn directed_evolution_field(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    field: &str,
) -> String {
    let entity = directed_evolution_entity(state, tenant, entity_type, entity_id).await;
    entity
        .state
        .fields
        .get(field)
        .or_else(|| {
            entity
                .state
                .fields
                .get(directed_evolution_lower_first(field))
        })
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

pub(super) async fn directed_evolution_ids_with_field(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    field: &str,
    expected: &str,
) -> Vec<String> {
    let mut matches = Vec::new();
    for id in state.server.list_entity_ids(tenant, entity_type) {
        let entity = directed_evolution_entity(state, tenant, entity_type, &id).await;
        if entity
            .state
            .fields
            .get(field)
            .or_else(|| {
                entity
                    .state
                    .fields
                    .get(directed_evolution_lower_first(field))
            })
            .and_then(|value| value.as_str())
            == Some(expected)
        {
            matches.push(id);
        }
    }
    matches.sort();
    matches
}

pub(super) async fn directed_evolution_wait_for_ids_with_field(
    state: &PlatformState,
    tenant: &TenantId,
    entity_type: &str,
    field: &str,
    expected: &str,
    expected_count: usize,
) -> Vec<String> {
    for _ in 0..200 {
        let ids =
            directed_evolution_ids_with_field(state, tenant, entity_type, field, expected).await;
        if ids.len() >= expected_count {
            return ids;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut snapshot = Vec::new();
    for id in state.server.list_entity_ids(tenant, entity_type) {
        let entity = directed_evolution_entity(state, tenant, entity_type, &id).await;
        snapshot.push((id, entity.state.status, entity.state.fields));
    }
    panic!(
        "timed out waiting for {expected_count} {entity_type} with {field}={expected}; found {snapshot:?}"
    );
}

pub(super) fn directed_evolution_lower_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub(super) async fn directed_evolution_run_work_item(
    state: &PlatformState,
    tenant: &TenantId,
    work_item_id: &str,
    role: &str,
    result_json: serde_json::Value,
) {
    let worker_run_id = format!("worker-run-{work_item_id}");
    directed_evolution_create(state, tenant, "WorkerRun", &worker_run_id).await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkerRun",
        &worker_run_id,
        "StartWorkerRun",
        serde_json::json!({
            "Role": role,
            "WorkItemId": work_item_id,
            "WorkerId": "test-codex-worker",
            "ProviderId": "local-codex",
            "AgentKind": "codex",
            "Model": "codex-local-test",
            "SessionId": "",
            "ParentSessionId": "directed-evolution-e2e",
            "CorrelationJson": "{}",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "ClaimWorkItem",
        serde_json::json!({
            "WorkerId": "test-codex-worker",
            "ClaimedBy": "codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "StartWorkItem",
        serde_json::json!({ "WorkerRunId": worker_run_id }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkerRun",
        &worker_run_id,
        "SucceedWorkerRun",
        serde_json::json!({
            "OutputJson": result_json.to_string(),
            "EvidenceArtifactId": "",
            "Summary": "test worker completed",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "SucceedWorkItem",
        serde_json::json!({
            "ResultJson": result_json.to_string(),
            "EvidenceArtifactId": "",
            "Summary": "test worker completed",
        }),
        false,
    )
    .await;
    directed_evolution_route_work_item_success(
        state,
        tenant,
        work_item_id,
        role,
        &worker_run_id,
        result_json.to_string(),
    )
    .await;
}

pub(super) async fn directed_evolution_fail_work_item(
    state: &PlatformState,
    tenant: &TenantId,
    work_item_id: &str,
    role: &str,
    failure_reason: &str,
) {
    let worker_run_id = format!("worker-run-{work_item_id}");
    directed_evolution_create(state, tenant, "WorkerRun", &worker_run_id).await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkerRun",
        &worker_run_id,
        "StartWorkerRun",
        serde_json::json!({
            "Role": role,
            "WorkItemId": work_item_id,
            "WorkerId": "test-codex-worker",
            "ProviderId": "local-codex",
            "AgentKind": "codex",
            "Model": "codex-local-test",
            "SessionId": "",
            "ParentSessionId": "directed-evolution-e2e",
            "CorrelationJson": "{}",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "ClaimWorkItem",
        serde_json::json!({
            "WorkerId": "test-codex-worker",
            "ClaimedBy": "codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "StartWorkItem",
        serde_json::json!({ "WorkerRunId": worker_run_id }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkerRun",
        &worker_run_id,
        "FailWorkerRun",
        serde_json::json!({
            "FailureReason": failure_reason,
            "EvidenceArtifactId": "",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItem",
        work_item_id,
        "FailWorkItem",
        serde_json::json!({
            "FailureReason": failure_reason,
            "EvidenceArtifactId": "",
        }),
        false,
    )
    .await;
    directed_evolution_route_work_item_failure(
        state,
        tenant,
        work_item_id,
        role,
        &worker_run_id,
        failure_reason,
    )
    .await;
}

async fn directed_evolution_route_work_item_success(
    state: &PlatformState,
    tenant: &TenantId,
    work_item_id: &str,
    role: &str,
    worker_run_id: &str,
    result_json: String,
) {
    let receipt_id = format!("work-item-receipt-{work_item_id}");
    let work_item = directed_evolution_entity(state, tenant, "WorkItem", work_item_id).await;
    let target_entity_type = work_item
        .state
        .fields
        .get("TargetEntityType")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let target_entity_id = work_item
        .state
        .fields
        .get("TargetEntityId")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let correlation_json = work_item
        .state
        .fields
        .get("CorrelationJson")
        .and_then(|value| value.as_str())
        .unwrap_or("{}")
        .to_string();

    directed_evolution_create(state, tenant, "WorkItemReceipt", &receipt_id).await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItemReceipt",
        &receipt_id,
        "RouteSucceededWorkItem",
        serde_json::json!({
            "WorkItemId": work_item_id,
            "Role": role,
            "TargetEntityType": target_entity_type,
            "TargetEntityId": target_entity_id,
            "WorkerRunId": worker_run_id,
            "ResultJson": result_json,
            "EvidenceArtifactId": "",
            "Summary": "test worker completed",
            "CorrelationJson": correlation_json,
        }),
        true,
    )
    .await;
}

async fn directed_evolution_route_work_item_failure(
    state: &PlatformState,
    tenant: &TenantId,
    work_item_id: &str,
    role: &str,
    worker_run_id: &str,
    failure_reason: &str,
) {
    let receipt_id = format!("work-item-receipt-{work_item_id}");
    let work_item = directed_evolution_entity(state, tenant, "WorkItem", work_item_id).await;
    let target_entity_type = work_item
        .state
        .fields
        .get("TargetEntityType")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let target_entity_id = work_item
        .state
        .fields
        .get("TargetEntityId")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let correlation_json = work_item
        .state
        .fields
        .get("CorrelationJson")
        .and_then(|value| value.as_str())
        .unwrap_or("{}")
        .to_string();

    directed_evolution_create(state, tenant, "WorkItemReceipt", &receipt_id).await;
    directed_evolution_dispatch(
        state,
        tenant,
        "WorkItemReceipt",
        &receipt_id,
        "RouteFailedWorkItem",
        serde_json::json!({
            "WorkItemId": work_item_id,
            "Role": role,
            "TargetEntityType": target_entity_type,
            "TargetEntityId": target_entity_id,
            "WorkerRunId": worker_run_id,
            "FailureReason": failure_reason,
            "EvidenceArtifactId": "",
            "CorrelationJson": correlation_json,
        }),
        true,
    )
    .await;
}

pub(super) fn directed_evolution_register_wasm_modules_for_test(
    state: &PlatformState,
    tenant: &TenantId,
) {
    let modules: [(&str, &[u8]); 3] = [
        (
            "signal_observer",
            include_bytes!(
                "../../../../../test-fixtures/directed-evolution-wasm/signal_observer.wasm"
            ),
        ),
        (
            "episode_orchestrator",
            include_bytes!(
                "../../../../../test-fixtures/directed-evolution-wasm/episode_orchestrator.wasm"
            ),
        ),
        (
            "work_item_result_router",
            include_bytes!(
                "../../../../../test-fixtures/directed-evolution-wasm/work_item_result_router.wasm"
            ),
        ),
    ];
    for (module_name, bytes) in modules {
        let hash = state
            .server
            .wasm_engine
            .compile_and_cache(bytes)
            .unwrap_or_else(|error| panic!("compile {module_name}: {error}"));
        state
            .server
            .wasm_module_registry
            .write()
            .expect("wasm registry lock")
            .register(tenant, module_name, &hash);
    }
}
