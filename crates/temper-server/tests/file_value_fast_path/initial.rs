//! Focused File value-path regression group.

use super::*;

#[tokio::test]
async fn create_file_with_initial_stream_content_projects_only_ready_content() {
    let (mut state, store) = build_turso_file_state("atomic-initial-content").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    let body = b"atomic initial File value";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-atomic-initial",
            serde_json::json!({
                "name": "atomic.md",
                "path": "/atomic.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-root",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("atomic initial File content write should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.sequence_nr, 3);
    assert_eq!(response.state.fields["name"], "atomic.md");
    assert_eq!(response.state.fields["content_hash"], expected_hash);
    assert_eq!(response.state.fields["has_content"], true);
    assert_eq!(response.state.fields["size_bytes"], body.len() as i64);

    let events = store
        .read_events("default:File:fl-atomic-initial", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actions, ["Created", "Create", "StreamUpdated"]);

    let indexed = state
        .read_file_stream_indexed(&tenant, "fl-atomic-initial")
        .await
        .expect("indexed read should see first bytes");
    assert_eq!(
        indexed,
        IndexedFileStreamRead::Content {
            content_hash: expected_hash.clone(),
            mime_type: "text/markdown".to_string(),
            bytes: body.to_vec(),
        }
    );
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test(start_paused = true)]
async fn atomic_initial_file_content_arms_timeout_without_later_access() {
    let seed = 234;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let (mut state, store) = build_sim_timed_file_state(seed);
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let file_id = "fl-timed-atomic-initial";
    let persistence_id = format!("default:File:{file_id}");

    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            file_id,
            serde_json::json!({}),
            b"timed atomic File value",
            "text/plain",
            &AgentContext::for_service("timed-file-test-writer"),
        )
        .await
        .expect("atomic initial File content write should succeed");
    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.sequence_nr, 2);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("File".to_string(), 1)],
        "the atomic File commit must arm its timeout without a later read"
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

    let journal = store.dump_journal(&persistence_id);
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "StreamUpdated", "TimeoutFail"],
        "the new File must time out without intervening entity access"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "the synthetic File creation path must deliver the timeout exactly once"
    );
}

#[tokio::test(start_paused = true)]
async fn atomic_initial_file_content_arms_timeout_when_projection_fails_after_commit() {
    let seed = 235;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let (mut state, store) =
        build_sim_timed_file_state_with_query_plane(seed, Some(Arc::new(FailingQueryPlane)));
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    let file_id = "fl-timed-projection-failure";
    let persistence_id = format!("default:File:{file_id}");

    let error = state
        .create_file_with_initial_stream_content(
            &tenant,
            file_id,
            serde_json::json!({}),
            b"timed File value before projection failure",
            "text/plain",
            &AgentContext::for_service("timed-file-projection-failure-test"),
        )
        .await
        .expect_err("the injected query projection write must fail");
    assert!(
        error.contains("query projection write failed"),
        "unexpected post-commit failure: {error}"
    );
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "StreamUpdated"],
        "the File journal commit precedes the injected projection failure"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("File".to_string(), 1)],
        "post-commit projection failure must not strand the durable timeout"
    );
    assert!(
        state.entity_exists(&tenant, "File", file_id),
        "the committed File remains indexed when projection publication fails"
    );
    let actor = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .cloned()
        .expect("the authoritative File actor materializes before projection");
    assert!(!actor.is_stopped());
    assert!(!actor.is_draining());

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
        vec!["Created", "StreamUpdated", "TimeoutFail"],
        "the committed File must time out without retry, access, or restart"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "the projection-fault path must deliver the timeout exactly once"
    );
}
