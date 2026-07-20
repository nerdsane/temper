//! Focused passivation timeout-anchor regression group.

use super::*;

#[tokio::test(start_paused = true)]
async fn legacy_with_specs_startup_index_rearms_timeout_without_traffic() {
    let seed = 213;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-index-timeout";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    sim_store
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
        .expect("seed the persisted legacy timed entity");

    let state = ServerState::with_storage_stack(
        ActorSystem::new("legacy-index-timeout"),
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
        "the startup scan must exercise the legacy transition-table fallback"
    );

    state.populate_index_from_store(&tenant).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "startup index population must eagerly spawn a persisted legacy timed entity"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "legacy startup must re-arm the durable timeout without entity traffic"
    );

    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "legacy startup must preserve the original timeout budget"
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
        .expect("read the legacy startup timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "legacy startup must fire the recovered timeout without a later request"
    );
}

#[tokio::test(start_paused = true)]
async fn slow_successful_pre_start_still_arms_initial_state_timeout() {
    let seed = 209;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "slow-successful-timeout-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    // Hold the bootstrap append beyond the complete actor-ask retry budget.
    // The actor remains live and eventually starts successfully, so stopped-
    // incarnation replacement cannot recover a lost one-shot hydration task.
    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    let mut state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "slow-successful-timeout-start",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    state.action_dispatch_timeout = std::time::Duration::from_millis(1);

    let actor_ref = state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the delayed timed actor");
    tokio::task::yield_now().await;

    // Step virtual time between task turns so every timeout/backoff in the
    // maximum supported 32-attempt policy is created and consumed while the
    // bootstrap append remains blocked. Each attempt needs at most one 800 ms
    // backoff turn and one 1 ms ask-timeout turn.
    for _ in 0..70 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sim_store.total_events(),
        0,
        "pre_start must still be waiting after the complete readiness-ask budget"
    );
    assert!(
        !actor_ref.is_stopped(),
        "a slow successful pre_start keeps its mailbox incarnation live"
    );
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "no timeout can be armed before the actor has recovered its state"
    );

    tokio::time::advance(std::time::Duration::from_secs(50)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1 {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "the delayed bootstrap append must eventually complete successfully"
    );

    // The arm must appear after late readiness without any entity request.
    for _ in 0..64 {
        if state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "late successful actor readiness must still arm exactly one initial-state timeout"
    );

    // The 120-second startup delay is charged against the original 600-second
    // budget, leaving exactly 480 seconds after readiness. Prove the timer is
    // neither early nor late and persists its transition before any read.
    tokio::time::advance(std::time::Duration::from_secs(479)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the recovered timeout must not fire before its durable deadline"
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
        .expect("read the no-traffic timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "late readiness must still durably fire the initial-state timeout without request traffic"
    );

    let recovered = state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("the actor remains readable after its recovered timeout fires");
    assert_eq!(recovered.state.status, "TimedOut");
}

#[tokio::test(start_paused = true)]
async fn queued_restarts_cannot_overtake_timeout_hydration_handshake() {
    const QUEUED_RESTART_BUDGET: usize = 320;

    let seed = 210;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "queued-restarts-timeout-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    let mut state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "queued-restarts-timeout-start",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    state.action_dispatch_timeout = std::time::Duration::from_millis(1);

    let actor_ref = state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the delayed timed actor");

    // Queue lifecycle work before either spawned task gets a turn. A readiness
    // signal that merely wakes a later hydration ask lets these restarts run
    // first and consume every ask attempt while the actor remains live.
    for _ in 0..QUEUED_RESTART_BUDGET {
        actor_ref
            .signal(SystemSignal::Restart)
            .expect("the bounded mailbox accepts the restart workload");
    }
    tokio::task::yield_now().await;

    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1 {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "the delayed initial-state event must commit before reconciliation"
    );

    // One restart is consumed per virtual-time step. This workload exceeds
    // the maximum 32-attempt ask schedule while staying below the fixed 1,000
    // message mailbox budget.
    for _ in 0..QUEUED_RESTART_BUDGET {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert!(
        !actor_ref.is_stopped(),
        "queued restarts must leave the actor incarnation live"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "startup reconciliation must be ordered ahead of already-queued mailbox work"
    );

    // The original deadline remains t=600: 120 seconds in initial startup and
    // 320 seconds draining restarts leave 160 seconds. Prove the transition is
    // durable before any entity request can provide a fallback reconciliation.
    tokio::time::advance(std::time::Duration::from_secs(159)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "queued lifecycle work must not move the original timeout deadline"
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
        .expect("read the no-traffic timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "queued restarts must not prevent the durable timeout transition"
    );

    let recovered = state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("the actor remains readable after its durable timeout");
    assert_eq!(recovered.state.status, "TimedOut");
}
