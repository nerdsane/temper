//! Faulted and out-of-band timeout hot-swap reconciliation regressions.

use super::*;

async fn wait_for_owner(state: &ServerState) {
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            return;
        }
    }
}

fn swap_to_timed(state: &ServerState, tenant: &TenantId) {
    let mut registry = state.registry.write().expect("registry lock");
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        common::CSDL_XML.to_string(),
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
}

#[tokio::test(start_paused = true)]
async fn hotswap_rescans_stale_index_and_retries_transient_typed_list_failure() {
    let seed = 235;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "out-of-band-first-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let reader = common::build_single_tenant_state_with_store(
        store.clone(),
        "out-of-band-timeout-reader",
        tenant.as_str(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    assert!(
        reader
            .list_entity_ids_lazy(&tenant, "InitialTimedTask")
            .await
            .is_empty(),
        "precondition: the reader caches an authoritative empty typed scan"
    );
    let writer = common::build_single_tenant_state_with_store(
        store.clone(),
        "out-of-band-timeout-writer",
        tenant.as_str(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );
    writer
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("a separate writer persists the entity after the reader's scan");
    assert_eq!(store.total_events(), 1);
    assert!(
        reader
            .list_entity_ids(&tenant, "InitialTimedTask")
            .is_empty()
    );

    store.fail_next_typed_lists(tenant.as_str(), "InitialTimedTask", 1);
    swap_to_timed(&reader, &tenant);
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if store.pending_typed_list_failures(tenant.as_str(), "InitialTimedTask") == 0 {
            break;
        }
    }
    assert_eq!(
        store.pending_typed_list_failures(tenant.as_str(), "InitialTimedTask"),
        0,
        "the first authoritative reconciliation scan must observe the injected fault"
    );
    assert!(
        reader.state_timeout_tracker.pending_snapshot().is_empty(),
        "a failed listing cannot pretend the stale in-memory index is complete"
    );

    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    wait_for_owner(&reader).await;
    assert_eq!(
        reader.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "the owned retry must freshly discover and arm the out-of-band entity"
    );

    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if store.total_events() == 2 {
            break;
        }
    }
    let journal = store
        .read_events(&actor_key, 0)
        .await
        .expect("read out-of-band hot-swap journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"]
    );
}

#[tokio::test(start_paused = true)]
async fn timed_swap_before_reconciler_start_is_caught_by_initial_sweep() {
    let seed = 236;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "swap-before-reconciler-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Created".to_string(),
                payload: serde_json::json!({
                    "action": "Created",
                    "from_status": "",
                    "to_status": "Running",
                    "timestamp": sim_now(),
                    "params": {}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed durable untimed entity");

    let mut registry = temper_server::registry::SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(common::CSDL_XML).expect("CSDL parse"),
        common::CSDL_XML.to_string(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("swap-before-reconciler-start"), registry);
    swap_to_timed(&state, &tenant);
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());

    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let _ = state
        .list_entity_ids_lazy(&tenant, "UnrelatedUntimedType")
        .await;
    wait_for_owner(&state).await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "subscription startup must sweep the already-current timed table"
    );
}

#[tokio::test(start_paused = true)]
async fn hotswap_retries_transient_actor_materialization_failure() {
    let seed = 237;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "materialization-retry-first-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let writer = common::build_single_tenant_state_with_store(
        store.clone(),
        "materialization-retry-writer",
        tenant.as_str(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );
    writer
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("persist untimed entity through a separate writer");

    let mut reader = common::build_single_tenant_state_with_store(
        store.clone(),
        "materialization-retry-reader",
        tenant.as_str(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );
    reader.action_dispatch_timeout = std::time::Duration::from_millis(1);
    let _ = reader
        .list_entity_ids_lazy(&tenant, "UnrelatedUntimedType")
        .await;
    store.fail_next_reads(&actor_key, 1_000);
    swap_to_timed(&reader, &tenant);

    for _ in 0..80 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        if store.pending_read_failures(&actor_key) < 1_000
            && !reader
                .actor_registry
                .read()
                .expect("actor registry lock")
                .contains_key(&actor_key)
        {
            break;
        }
    }
    assert!(
        store.pending_read_failures(&actor_key) < 1_000,
        "the first materialization attempt must observe the injected read fault"
    );
    assert!(
        reader.state_timeout_tracker.pending_snapshot().is_empty(),
        "failed materialization cannot claim timeout ownership"
    );

    store.fail_next_reads(&actor_key, 0);
    for _ in 0..80 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        wait_for_owner(&reader).await;
        if reader.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(
        reader.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "the owned reconciliation retry must recover failed actor materialization"
    );

    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if store.total_events() == 2 {
            break;
        }
    }
    let journal = store
        .read_events(&actor_key, 0)
        .await
        .expect("read materialization-retry journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"]
    );
}

#[tokio::test(start_paused = true)]
async fn remove_and_readd_does_not_lose_new_controller_timeout_reconciliation() {
    let seed = 239;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "readded-controller-first-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        store.clone(),
        "readded-controller-timeout",
        tenant.as_str(),
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );
    state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("persist entity and start the reconciliation subscriber");

    // Queue an old-controller v2 signal immediately before replacing the type
    // with a fresh timed controller at v1. The worker must not use a
    // controller-local version to discard the replacement's signal.
    swap_to_timed(&state, &tenant);
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry.register_tenant(
            tenant.as_str(),
            parse_csdl(common::CSDL_XML).expect("CSDL parse"),
            common::CSDL_XML.to_string(),
            &[],
        );
        registry.register_tenant(
            tenant.as_str(),
            parse_csdl(common::CSDL_XML).expect("CSDL parse"),
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }

    wait_for_owner(&state).await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "the replacement controller must retain its authoritative timeout sweep"
    );

    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if store.total_events() == 2 {
            break;
        }
    }
    let journal = store
        .read_events(&actor_key, 0)
        .await
        .expect("read replacement-controller journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"]
    );
}
