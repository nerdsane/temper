//! Focused File value-path regression group.

use super::*;

#[tokio::test]
async fn put_value_on_new_file_is_one_atomic_append() {
    // ARN-87 write-side: a brand-new File's first `$value` PUT must commit as ONE
    // atomic create-with-content append. The pre-fix path spawned the actor
    // (whose `pre_start` persists an empty bootstrap `Created`) and only THEN
    // dispatched `StreamUpdated` as a SEPARATE append — leaving the File durable
    // and projection-visible with an empty `$value` between the two appends. This
    // test reads the durable journal and asserts there is no such window: the
    // create and the content land together (Created [+ Create] + StreamUpdated),
    // and the highest-sequence event carries content (StreamUpdated), never a
    // lone empty `Created`.
    let (mut state, store) = build_turso_file_state("new-file-atomic-append").await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body = b"brand new file value, one append";

    let response = build_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-new-atomic')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let events = store
        .read_events("default:File:fl-new-atomic", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();

    // The whole journal is a single atomic boundary: the create and the first
    // content arrive together. There is NO lone bootstrap `Created` that is later
    // followed by a separate `StreamUpdated` append.
    assert_eq!(
        actions,
        ["Created", "StreamUpdated"],
        "new-File PUT must be one atomic create-with-content append, got {actions:?}"
    );

    // The highest-sequence event carries content — there is no durable state
    // whose max-seq event is an empty `Created`.
    let last = events.last().expect("journal has events");
    assert_eq!(last.event_type, "StreamUpdated");
    assert_eq!(last.sequence_nr, 2);

    // And the bootstrap `Created` never carried (empty) content of its own that
    // could be read before `StreamUpdated` landed.
    let created = events
        .iter()
        .find(|event| event.event_type == "Created")
        .expect("created event present");
    assert!(
        created
            .payload
            .get("params")
            .and_then(|params| params.get("content_hash"))
            .is_none(),
        "the create event must not carry content; content arrives with StreamUpdated"
    );
}

#[tokio::test]
async fn new_file_value_put_is_read_after_write_consistent() {
    // The window the fix closes is observable through reads: immediately after a
    // brand-new File `$value` PUT, the projection must already reflect content —
    // never an empty `has_content=false` / no-`content_hash` row.
    let (mut state, _store) = build_turso_file_state("new-file-raw").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body = b"read after write consistency";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));

    let response = build_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-raw-consistent')/$value")
                .header("content-type", "text/markdown")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Read immediately: the very first observable state already has content.
    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-raw-consistent")
        .await
        .expect("new File should be readable right after the PUT");
    assert_eq!(entity.state.status, "Ready");
    assert_eq!(entity.state.fields["content_hash"], expected_hash);
    assert_eq!(entity.state.fields["has_content"], true);
    assert_eq!(entity.state.fields["mime_type"], "text/markdown");

    // The indexed `$value` read sees the bytes — there is no empty row to observe.
    let indexed = state
        .read_file_stream_indexed(&tenant, "fl-raw-consistent")
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
}

#[tokio::test]
async fn concurrent_new_file_value_puts_yield_one_204_one_409() {
    // Two concurrent brand-new PUTs to the SAME id: the atomic single-append
    // create is guarded by the journal's expected-sequence (0) check, so exactly
    // one wins (204) and the other is rejected as a concurrency violation (409
    // ActionRejected). Neither leaves an empty `$value` behind.
    let (mut state, store) = build_turso_file_state("new-file-concurrent").await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    let body_a = b"writer A bytes";
    let body_b = b"writer B different bytes";

    let app = build_router(state.clone());
    let put_a = app.clone().oneshot(
        Request::put("/tdata/Files('fl-race')/$value")
            .header("content-type", "text/plain")
            .body(Body::from(body_a.as_slice()))
            .expect("request A"),
    );
    let put_b = app.oneshot(
        Request::put("/tdata/Files('fl-race')/$value")
            .header("content-type", "text/plain")
            .body(Body::from(body_b.as_slice()))
            .expect("request B"),
    );
    let (res_a, res_b) = tokio::join!(put_a, put_b);
    let status_a = res_a.expect("route A responds").status();
    let status_b = res_b.expect("route B responds").status();

    let mut statuses = [status_a, status_b];
    statuses.sort_by_key(|status| status.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::NO_CONTENT, StatusCode::CONFLICT],
        "exactly one new-File PUT must succeed (204) and the other conflict (409), got {statuses:?}"
    );

    // The winner left a single coherent journal: create + content, no second
    // bootstrap, no empty-value remnant from the loser.
    let events = store
        .read_events("default:File:fl-race", 0)
        .await
        .expect("read File journal");
    let actions = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        ["Created", "StreamUpdated"],
        "the winning create must be the only durable history, got {actions:?}"
    );
}

#[tokio::test]
async fn existing_file_value_put_routes_through_update_unchanged() {
    // Regression guard: PUT `$value` on an EXISTING File still routes through the
    // update path (a fresh `StreamUpdated` append that bumps the version), proving
    // the ARN-87 new-File fast path did not change behavior for updates.
    let (mut state, store) = build_turso_file_state("existing-file-update").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    // First write: brand-new File via the atomic create-with-content path.
    let first = b"version one bytes";
    let first_hash = format!("sha256:{:x}", Sha256::digest(first));
    let response = build_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-update')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(first.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let after_create = store
        .read_events("default:File:fl-update", 0)
        .await
        .expect("read File journal after create");
    assert_eq!(
        after_create
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["Created", "StreamUpdated"],
    );

    // Second write to the SAME (now existing) File: must go through the update
    // path and append another StreamUpdated, bumping the version.
    let second = b"version two bytes, larger payload";
    let second_hash = format!("sha256:{:x}", Sha256::digest(second));
    assert_ne!(first_hash, second_hash);
    let response = build_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-update')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(second.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let expected_etag = format!("\"{second_hash}\"");
    assert_eq!(
        response.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(expected_etag.as_str()),
        "update must return the new content hash"
    );

    let after_update = store
        .read_events("default:File:fl-update", 0)
        .await
        .expect("read File journal after update");
    let update_actions = after_update
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        update_actions,
        ["Created", "StreamUpdated", "StreamUpdated"],
        "existing-File PUT must append a fresh StreamUpdated (no re-create), got {update_actions:?}"
    );

    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-update")
        .await
        .expect("File state should reflect the update");
    assert_eq!(entity.state.fields["content_hash"], second_hash);
    assert_eq!(entity.state.fields["has_content"], true);
    // The StreamUpdated effect increments `version_count` once per append; two
    // content writes => version 2.
    assert_eq!(
        entity.state.counters.get("version_count").copied(),
        Some(2),
        "each content write bumps the version"
    );
}

#[tokio::test]
async fn read_file_stream_indexed_reports_stale_index_when_blob_is_missing() {
    let (state, store) = build_turso_state("stale-index").await;
    let tenant = TenantId::default();
    let content_hash = "sha256:missing-blob";

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-stale")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(
        read,
        IndexedFileStreamRead::StaleIndex {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
        }
    );
}

// ─── Fix #1/#2: a Frozen Workspace rejects new File writes ──────────────
