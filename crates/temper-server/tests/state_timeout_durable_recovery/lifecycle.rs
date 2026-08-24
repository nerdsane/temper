use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_supersedes_the_old_clock_without_extending_the_new_deadline() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-reset-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let inspection_store = BoxedEventStore::new(store.clone());
    let state = build_state("timeout-reset", store);
    state
        .get_or_create_tenant_entity(&tenant, "Ticket", "ticket-reset", serde_json::json!({}))
        .await
        .expect("create timed entity");

    tokio::time::sleep(Duration::from_millis(600)).await; // determinism-ok: exercise reset between two durable deadlines
    state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "ticket-reset",
            "Heartbeat",
            serde_json::json!({}),
            &temper_server::request_context::AgentContext::default(),
        )
        .await
        .expect("reset action");
    tokio::time::sleep(Duration::from_millis(550)).await; // determinism-ok: pass old deadline but remain before reset deadline
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Ticket", "ticket-reset")
            .await
            .expect("load reset entity")
            .state
            .status,
        "Open",
        "the superseded clock must not fire"
    );
    assert_eq!(
        wait_for_status(
            &state,
            &tenant,
            "ticket-reset",
            "InProgress",
            Duration::from_secs(5),
        )
        .await,
        "InProgress"
    );
    wait_for_delivery_status(
        &inspection_store,
        &tenant,
        temper_server::trigger::delivery::ReactionDeliveryStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    let records = temper_server::trigger::delivery::list_delivery_records(
        &inspection_store,
        tenant.as_str(),
        10,
    )
    .await
    .expect("delivery records");
    assert!(records.iter().any(|(record, _)| {
        record.status == temper_server::trigger::delivery::ReactionDeliveryStatus::Skipped
            && record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("superseded"))
    }));
    assert_eq!(
        records
            .iter()
            .filter(|(record, _)| {
                record.status == temper_server::trigger::delivery::ReactionDeliveryStatus::Succeeded
            })
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_occurrences_is_a_durable_entity_declaration_budget() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-occurrences-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let inspection_store = BoxedEventStore::new(store.clone());
    let state = build_state("timeout-occurrences", store);
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Ticket",
            "ticket-occurrences",
            serde_json::json!({}),
        )
        .await
        .expect("create timed entity");
    assert_eq!(
        wait_for_status(
            &state,
            &tenant,
            "ticket-occurrences",
            "InProgress",
            Duration::from_secs(5),
        )
        .await,
        "InProgress"
    );

    // Recreate the exact crash window where the target event + receipt
    // committed but the worker died before acknowledging lifecycle success.
    let (mut first_record, first_sequence) = wait_for_delivery_status(
        &inspection_store,
        &tenant,
        temper_server::trigger::delivery::ReactionDeliveryStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    first_record.status = temper_server::trigger::delivery::ReactionDeliveryStatus::Dispatching;
    first_record.lease_expires_at =
        Some(temper_runtime::scheduler::sim_now() + chrono::Duration::seconds(30));
    temper_server::trigger::delivery::append_delivery_record(
        &inspection_store,
        first_sequence,
        &first_record,
    )
    .await
    .expect("persist simulated pre-ack crash lifecycle");
    drop(state);
    let state = build_state("timeout-occurrences-restart", open_store(&db_url).await);
    state.populate_index_from_store(&tenant).await;

    state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "ticket-occurrences",
            "Reopen",
            serde_json::json!({}),
            &temper_server::request_context::AgentContext::default(),
        )
        .await
        .expect("re-enter timed state");
    tokio::time::sleep(Duration::from_millis(1_250)).await; // determinism-ok: pass second clock deadline
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Ticket", "ticket-occurrences")
            .await
            .expect("load re-entered entity")
            .state
            .status,
        "Open",
        "default max_occurrences=1 must suppress a second declaration firing"
    );
    let records = temper_server::trigger::delivery::list_delivery_records(
        &inspection_store,
        tenant.as_str(),
        10,
    )
    .await
    .expect("delivery records");
    assert!(records.iter().any(|(record, _)| {
        record.status == temper_server::trigger::delivery::ReactionDeliveryStatus::Skipped
            && record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("occurrence budget exhausted"))
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_the_timed_state_cancels_delivery_after_passivation_safe_recovery() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-state-exit-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let state = build_state("timeout-state-exit", store);
    state
        .get_or_create_tenant_entity(&tenant, "Ticket", "ticket-closed", serde_json::json!({}))
        .await
        .expect("create timed entity");
    state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "ticket-closed",
            "Close",
            serde_json::json!({}),
            &temper_server::request_context::AgentContext::default(),
        )
        .await
        .expect("leave timed state");
    let actor_key = "tenant-a:Ticket:ticket-closed".to_string();
    state
        .last_accessed
        .write()
        .expect("last-access lock")
        .insert(
            actor_key.clone(),
            temper_runtime::scheduler::sim_now() - chrono::Duration::seconds(600),
        );
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "test must prove the actor was actually passivated"
    );
    tokio::time::sleep(Duration::from_millis(1_200)).await; // determinism-ok: pass original absolute deadline
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Ticket", "ticket-closed")
            .await
            .expect("hydrate closed entity")
            .state
            .status,
        "Closed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_deadline_survives_hard_restart_without_fresh_budget() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-pending-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    run_and_hard_kill_generation_a(&db_url, "ticket-pending");

    tokio::time::sleep(Duration::from_millis(400)).await; // determinism-ok: simulated downtime
    let state = build_state("timeout-generation-b", open_store(&db_url).await);
    state.populate_index_from_store(&tenant).await;

    let started = Instant::now(); // determinism-ok: integration-test latency assertion only
    let status = wait_for_status(
        &state,
        &tenant,
        "ticket-pending",
        "InProgress",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, "InProgress");
    assert!(
        started.elapsed() < Duration::from_millis(950),
        "restart must preserve the original deadline instead of granting a fresh second"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overdue_deadline_fires_promptly_after_hard_restart() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-overdue-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    run_and_hard_kill_generation_a(&db_url, "ticket-overdue");

    tokio::time::sleep(Duration::from_millis(1_200)).await; // determinism-ok: simulated downtime
    let state = build_state("timeout-generation-b-overdue", open_store(&db_url).await);
    state.populate_index_from_store(&tenant).await;

    let started = Instant::now(); // determinism-ok: integration-test latency assertion only
    let status = wait_for_status(
        &state,
        &tenant,
        "ticket-overdue",
        "InProgress",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, "InProgress");
    assert!(
        started.elapsed() < Duration::from_millis(750),
        "overdue recovery must not wait another full timeout budget"
    );
}
