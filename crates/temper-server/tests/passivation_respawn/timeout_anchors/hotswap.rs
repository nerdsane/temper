//! Focused passivation timeout-anchor regression group.

use super::*;

async fn wait_for_timeout_owner(state: &ServerState) {
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn hotswap_before_pre_start_cannot_skip_initial_timeout_hydration() {
    let seed = 211;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "hotswap-before-pre-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "hotswap-before-pre-start",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    // Actor tasks cannot poll until this synchronous test body yields. Spawn
    // under an untimed table, then replace the same live table before pre_start
    // snapshots it. Startup therefore observes the timed definition even
    // though spawn-time admission originally observed the untimed definition.
    state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the actor before its first task poll");
    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }

    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1
            && state.state_timeout_tracker.pending_snapshot()
                == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "pre_start must commit the initial event under the hot-swapped table"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "a timeout added before pre_start must be hydrated without entity traffic"
    );

    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the hot-swapped timeout must not fire before its original deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the hot-swap timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "the pre-start hot-swap must still durably fire without a later read"
    );
}

#[tokio::test(start_paused = true)]
async fn hotswap_after_pre_start_table_snapshot_uses_actor_clock_identity() {
    let seed = 214;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "hotswap-after-table-snapshot";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "hotswap-after-table-snapshot",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    // pre_start snapshots the untimed table before its bootstrap append. Hold
    // that append so the hot-swap lands strictly after the snapshot but before
    // startup publishes the recovered state to timeout hydration.
    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the actor under the untimed table");
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.pending_append_delays(&actor_key) == 0 {
            break;
        }
    }
    assert_eq!(
        sim_store.pending_append_delays(&actor_key),
        0,
        "the delayed append must start after pre_start captured its table"
    );
    assert_eq!(
        sim_store.total_events(),
        0,
        "the bootstrap event must remain inside the controlled append window"
    );

    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }

    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1
            && state.state_timeout_tracker.pending_snapshot()
                == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "hydration must arm the newly introduced timeout without entity traffic"
    );

    tokio::time::advance(std::time::Duration::from_secs(479)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the hot-swapped timeout must retain the Created-event deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the post-snapshot hot-swap journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "a post-snapshot hot-swap must still fire exactly one durable timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn ready_untimed_entity_arms_first_timeout_after_registry_hotswap_without_traffic() {
    let seed = 232;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "ready-first-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "ready-first-timeout",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    let ready = state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("untimed actor reaches steady state before the hot-swap");
    assert_eq!(ready.state.status, "Running");
    assert_eq!(sim_store.total_events(), 1);
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());

    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }
    wait_for_timeout_owner(&state).await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "a steady-state entity must observe a newly added timeout without another dispatch"
    );

    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read steady-state hot-swap journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"]
    );
}

#[tokio::test(start_paused = true)]
async fn direct_swap_controller_arms_first_timeout_without_traffic() {
    let seed = 233;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "direct-swap-first-timeout";
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "direct-swap-first-timeout",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("untimed actor reaches steady state before direct swap");
    assert_eq!(sim_store.total_events(), 1);
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());

    let result = {
        let registry = state.registry.read().expect("registry lock");
        let spec = registry
            .get_spec(&tenant, "InitialTimedTask")
            .expect("registered entity spec");
        spec.swap_controller()
            .swap(temper_jit::table::TransitionTable::from_ioa_source(
                INITIAL_TIMED_TASK_IOA,
            ))
    };
    assert!(
        matches!(result, temper_jit::swap::SwapResult::Success { .. }),
        "direct table swap must succeed"
    );
    wait_for_timeout_owner(&state).await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "the documented direct swap API must publish the same timeout notification"
    );
}

#[tokio::test(start_paused = true)]
async fn lazy_durable_entity_arms_first_timeout_after_registry_hotswap() {
    let seed = 234;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "lazy-first-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "lazy-first-timeout",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("persist the entity under the untimed definition");
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "the durable entity must be lazy when the new timeout is deployed"
    );

    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }
    wait_for_timeout_owner(&state).await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "hot-deploy must discover and reconcile a durable entity with no live actor"
    );
    assert!(
        state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "timeout reconciliation must materialize the durable entity"
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_with_specs_initial_timeout_arms_without_spec_registry() {
    let seed = 212;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-with-specs-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = ServerState::with_storage_stack(
        ActorSystem::new("legacy-with-specs-timeout"),
        parse_csdl(common::CSDL_XML).expect("CSDL parse"),
        common::CSDL_XML.to_string(),
        BTreeMap::from([(
            "InitialTimedTask".to_string(),
            INITIAL_TIMED_TASK_IOA.to_string(),
        )]),
        StorageStack::from_sim(sim_store.clone(), None),
    )
    .expect("build the legacy single-tenant server state");
    assert!(
        state
            .registry
            .read()
            .expect("registry lock")
            .get_spec(&tenant, "InitialTimedTask")
            .is_none(),
        "with_specs must exercise the legacy transition-table fallback"
    );

    state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the legacy timed actor");
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1
            && state.state_timeout_tracker.pending_snapshot()
                == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "legacy pre_start must commit the initial event"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "legacy timeout declarations must arm without a SpecRegistry entry"
    );

    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the legacy timeout must not fire before its durable deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the legacy timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "legacy hydration must durably fire without a later entity read"
    );
}
