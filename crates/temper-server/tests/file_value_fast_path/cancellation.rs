//! Cancellation safety after an atomic File journal commit.

use super::*;

#[tokio::test(start_paused = true)]
async fn request_cancellation_cannot_abort_committed_file_reconciliation() {
    let seed = 254;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let (mut state, store) = build_sim_timed_file_state(seed);
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let file_id = "fl-cancelled-after-commit";
    let persistence_id = format!("default:File:{file_id}");

    let initial_append_delay =
        store.inject_observed_append_delay(&persistence_id, std::time::Duration::from_secs(10));
    let racing_append_delay =
        store.inject_observed_append_delay(&persistence_id, std::time::Duration::from_secs(20));
    let request_state = state.clone();
    let request_tenant = tenant.clone();
    let request_file_id = file_id.to_string();
    let mut request = tokio::spawn(async move {
        let body = b"durable bytes survive request cancellation".to_vec();
        let agent = AgentContext::for_service("cancelled-file-request");
        request_state
            .create_file_with_initial_stream_content(
                &request_tenant,
                &request_file_id,
                serde_json::json!({}),
                &body,
                "text/plain",
                &agent,
            )
            .await
    });
    tokio::select! {
        observed = initial_append_delay => {
            observed.expect("the synthetic File append must consume its controlled delay");
        }
        result = &mut request => {
            panic!("File request finished before its append delay: {result:?}");
        }
    }
    assert_eq!(
        store.pending_append_delays(&persistence_id),
        1,
        "the synthetic File append must consume the first controlled delay"
    );

    let stale_actor = state
        .get_or_spawn_tenant_actor(&tenant, "File", file_id)
        .expect("publish the pre-commit File incarnation");
    let stale_uid = stale_actor.id().uid;
    let action_state = state.clone();
    let action_tenant = tenant.clone();
    let action_file_id = file_id.to_string();
    let mut actor_action = tokio::spawn(async move {
        let agent = AgentContext::for_service("cancelled-file-racing-actor");
        action_state
            .dispatch_tenant_action(
                &action_tenant,
                "File",
                &action_file_id,
                "StreamUpdated",
                serde_json::json!({
                    "content_hash": "sha256:racing",
                    "size_bytes": 1,
                    "mime_type": "text/plain",
                    "version_number": 2,
                    "previous_version_id": "",
                    "created_by": "cancelled-file-racing-actor"
                }),
                &agent,
            )
            .await
    });
    tokio::select! {
        observed = racing_append_delay => {
            observed.expect("the racing actor append must consume its controlled delay");
        }
        result = &mut actor_action => {
            panic!("actor action finished before its append delay: {result:?}");
        }
        result = &mut request => {
            panic!("File request finished before its commit: {result:?}");
        }
    }
    assert_eq!(store.pending_append_delays(&persistence_id), 0);

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    for _ in 0..128 {
        if store.dump_journal(&persistence_id).len() == 2 {
            break;
        }
        assert!(
            !request.is_finished(),
            "File reconciliation did not await the stale actor"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "StreamUpdated"],
        "the cancellation point must be after the durable File commit"
    );
    assert!(
        !request.is_finished(),
        "the request must still be awaiting stale-actor reconciliation"
    );

    request.abort();
    let cancellation = request.await.expect_err("the request task is cancelled");
    assert!(cancellation.is_cancelled());

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    let _ = actor_action
        .await
        .expect("racing actor task remains alive after request cancellation");
    for _ in 0..256 {
        let replacement_is_ready = state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .is_some_and(|actor| actor.id().uid != stale_uid && !actor.is_draining());
        if replacement_is_ready
            && state.state_timeout_tracker.pending_snapshot() == vec![("File".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("File".to_string(), 1)],
        "the detached File reconciliation must publish the durable timeout owner"
    );
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .is_some_and(|actor| actor.id().uid != stale_uid && !actor.is_draining()),
        "the detached reconciliation must replace the stale File incarnation"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if store
            .dump_journal(&persistence_id)
            .iter()
            .any(|event| event.event_type == "TimeoutFail")
        {
            break;
        }
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "StreamUpdated", "StreamUpdated", "TimeoutFail"],
        "request cancellation cannot strand the newer File timeout"
    );
}
