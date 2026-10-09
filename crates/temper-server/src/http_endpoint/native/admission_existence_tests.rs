//! Native and WASM route admission share the external OData existence boundary.
use super::*;
use temper_runtime::{persistence::EventStore, scheduler::install_deterministic_context};
use temper_store_sim::SimEventStore;

#[derive(Clone, Copy, Debug)]
enum Entry {
    Native,
    Wasm,
    OData,
}
const ENTRIES: [Entry; 3] = [Entry::Native, Entry::Wasm, Entry::OData];
const KEY: &str = "default:Target:one";

async fn persisted_fixture(store: &SimEventStore, entry: Entry) -> (ServerState, Arc<AtomicUsize>) {
    let (mut state, count) = fixture();
    state.set_storage_stack(crate::StorageStack::from_sim(store.clone(), None));
    match entry {
        Entry::Native => install(&state, config()).await,
        Entry::Wasm => {
            // A trap makes accidental guest execution observable, without I/O.
            let wasm = r#"(module (memory (export "memory") 1)
                (func (export "run") (param i32 i32) (result i32) unreachable))"#;
            let hash = state
                .wasm_engine
                .compile_and_cache(wasm.as_bytes())
                .unwrap();
            state.wasm_module_registry.write().unwrap().register(
                &TenantId::default(),
                "must-not-run",
                &hash,
            );
            let route = crate::http_endpoint::route_from_entity_fields("wasm-endpoint", &json!({
                "PathPrefix":"/things/{id}/tdata", "Methods":"POST", "IntegrationModule":"must-not-run",
                "RequiresAuth":true, "AdmissionActions":config()["actions"].to_string()
            })).unwrap();
            state
                .http_endpoint_tables
                .table_for(&TenantId::default())
                .await
                .replace(vec![route])
                .await;
        }
        Entry::OData => {}
    }
    (state, count)
}

async fn call(state: &ServerState, entry: Entry, user: &str) -> (StatusCode, Value) {
    let mut req = request(user);
    if matches!(entry, Entry::OData) {
        *req.uri_mut() = "/tdata/Targets('one')/Example.Serve".parse().unwrap();
        *req.body_mut() = Body::from("{}");
    }
    let response = crate::build_router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn assert_no_target(state: &ServerState, store: &SimEventStore, count: &AtomicUsize) {
    assert!(
        !state.entity_exists(&TenantId::default(), "Target", "one"),
        "index published a target"
    );
    assert_eq!(
        state.active_actor_count(),
        0,
        "admission materialized an actor"
    );
    assert!(store.dump_journal(KEY).is_empty());
    assert!(store.load_snapshot(KEY).await.unwrap().is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(state.http_stream_registry.handle_count().await, 0);
}

#[tokio::test]
async fn missing_target_is_not_created_by_native_wasm_or_odata_admission() {
    for seed in 52920..52923 {
        let (_guard, _, _) = install_deterministic_context(seed);
        for entry in ENTRIES {
            let store = SimEventStore::no_faults(seed);
            let (state, count) = persisted_fixture(&store, entry).await;
            let (status, body) = call(&state, entry, "alice").await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "seed={seed} entry={entry:?}: {body}"
            );
            assert_eq!(body["error"]["code"], "EntityNotFound");
            assert_no_target(&state, &store, &count).await;
        }
    }
}

#[tokio::test]
async fn cedar_denial_precedes_missing_target_disclosure_on_all_admission_entries() {
    for seed in 52920..52923 {
        let (_guard, _, _) = install_deterministic_context(seed);
        for entry in ENTRIES {
            let store = SimEventStore::no_faults(seed);
            let (state, count) = persisted_fixture(&store, entry).await;
            let (status, body) = call(&state, entry, "bob").await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "seed={seed} entry={entry:?}: {body}"
            );
            assert_ne!(body["error"]["code"], "EntityNotFound");
            assert_no_target(&state, &store, &count).await;
        }
    }
}

#[tokio::test]
async fn deleted_targets_are_not_resurrected_by_native_wasm_or_odata_admission() {
    for seed in 52920..52923 {
        let (_guard, _, _) = install_deterministic_context(seed);
        for entry in ENTRIES {
            let store = SimEventStore::no_faults(seed);
            let (writer, writer_count) = persisted_fixture(&store, entry).await;
            create_target(&writer, "one").await;
            let (stale, stale_count) = persisted_fixture(&store, entry).await;
            stale
                .get_tenant_entity_state(&TenantId::default(), "Target", "one")
                .await
                .unwrap();
            writer
                .delete_tenant_entity(&TenantId::default(), "Target", "one")
                .await
                .unwrap();
            let before = serde_json::to_value(store.dump_journal(KEY)).unwrap();
            assert_eq!(before.as_array().unwrap().len(), 2);
            assert_eq!(before[1]["event_type"], "Deleted");
            let (cold, cold_count) = persisted_fixture(&store, entry).await;
            for (state, count) in [
                (&writer, &writer_count),
                (&stale, &stale_count),
                (&cold, &cold_count),
            ] {
                let (status, body) = call(state, entry, "alice").await;
                assert_eq!(
                    status,
                    StatusCode::NOT_FOUND,
                    "seed={seed} entry={entry:?}: {body}"
                );
                assert_eq!(body["error"]["code"], "EntityNotFound");
                let (status, _) = call(state, entry, "bob").await;
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert_eq!(count.load(Ordering::SeqCst), 0);
                assert_eq!(state.http_stream_registry.handle_count().await, 0);
            }
            assert_eq!(
                serde_json::to_value(store.dump_journal(KEY)).unwrap(),
                before
            );
            assert_eq!(cold.active_actor_count(), 0);
            assert!(!cold.entity_exists(&TenantId::default(), "Target", "one"));
        }
    }
}

#[tokio::test]
async fn unavailable_or_corrupt_authority_is_not_missing_on_any_admission_entry() {
    for seed in 52920..52923 {
        let (_guard, _, _) = install_deterministic_context(seed);
        for entry in ENTRIES {
            for corrupt in [false, true] {
                let store = SimEventStore::no_faults(seed);
                if corrupt {
                    store
                        .save_snapshot(KEY, 1, b"invalid snapshot")
                        .await
                        .unwrap();
                } else {
                    store.fail_next_reads(KEY, 1);
                }
                let before = store.load_snapshot(KEY).await.unwrap();
                let (state, count) = persisted_fixture(&store, entry).await;
                let (status, body) = call(&state, entry, "alice").await;
                assert_eq!(
                    status,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "seed={seed} entry={entry:?}: {body}"
                );
                assert_eq!(body["error"]["code"], "ReadError");
                assert!(!state.entity_exists(&TenantId::default(), "Target", "one"));
                assert_eq!(state.active_actor_count(), 0);
                assert!(store.dump_journal(KEY).is_empty());
                assert_eq!(store.load_snapshot(KEY).await.unwrap(), before);
                assert_eq!(count.load(Ordering::SeqCst), 0);
                assert_eq!(state.http_stream_registry.handle_count().await, 0);
            }
        }
    }
}
