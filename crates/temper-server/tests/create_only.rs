//! Ordinary external collection POST is create-only (temper#529 V11): a
//! stream with ANY existing history -- live or tombstoned -- conflicts with
//! 409, never a silent 201 for someone else's write and never a
//! resurrection of a deleted id. Trusted internal get-or-create callers
//! (bootstrap, file/stream init, internal dispatch) are a distinct
//! operation and keep their existing semantics unchanged.
mod common;

use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{
    persistence::{EventStore, PersistenceError},
    scheduler::install_deterministic_context,
    tenant::TenantId,
};
use temper_server::{
    ServerState, StorageStack,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
    storage::{QueryPlaneStore, QueryProjectionFieldsRow},
};
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Governed, rule-bearing Order (goes through the actor path).
fn governed_fixture(store: SimEventStore, tenant: &str, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = temper_spec::csdl::parse_csdl(common::CSDL_XML).unwrap();
    registry.register_tenant(
        tenant,
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let result = temper_verify::cascade::VerificationCascade::from_ioa(common::ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(result.all_passed);
    registry.set_verification_status(
        &TenantId::new(tenant),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: result.all_passed,
            levels: vec![],
            verified_at: "2026-09-07T00:00:00Z".into(),
        }),
    );
    let system = temper_runtime::ActorSystem::new(name);
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .unwrap();
    assert!(
        !state.is_pg_actor_backed(&TenantId::new(tenant), "Order"),
        "fixture must stay on the non-PG create-only path"
    );
    state
}

#[derive(Default)]
struct NoopQueryPlane;

#[async_trait::async_trait]
impl QueryPlaneStore for NoopQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _kind: &str,
        _id: &str,
        _status: &str,
        _fields: &Value,
        _state: &Value,
        _sequence: u64,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }
    async fn remove_projection(&self, _: &str, _: &str, _: &str) -> Result<(), PersistenceError> {
        Ok(())
    }
    async fn query_field_index(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }
    async fn load_projection_fields_many(
        &self,
        _: &str,
        _: &str,
        _: &[String],
        _: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

/// Data-only-eligible fixture: same Order CSDL entity type, but the IOA is
/// truncated before `[[action]]` so the transition table has zero rules --
/// the exact eligibility test `try_create_data_only_tenant_entity` applies
/// (see `router_test.rs`'s
/// `test_data_only_create_fast_path_declines_action_bearing_entities`).
fn data_only_fixture(store: SimEventStore, tenant: &str, name: &str) -> ServerState {
    let csdl = temper_spec::csdl::parse_csdl(common::CSDL_XML).unwrap();
    let data_only_ioa = common::ORDER_IOA.split("[[action]]").next().unwrap();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant,
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", data_only_ioa)],
    );
    registry.set_verification_status(
        &TenantId::new(tenant),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-07T00:00:00Z".into(),
        }),
    );
    let system = temper_runtime::ActorSystem::new(name);
    let mut state = ServerState::from_registry(system, registry);
    let mut stack = StorageStack::from_sim(store, None);
    stack.query_plane = Some(std::sync::Arc::new(NoopQueryPlane));
    state.set_storage_stack(stack);
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .unwrap();
    assert!(
        !state.is_pg_actor_backed(&TenantId::new(tenant), "Order"),
        "fixture must stay on the non-PG create-only path"
    );
    state
}

async fn post(state: &ServerState, path: &str, body: Value) -> (StatusCode, Value) {
    post_tenant(state, "default", path, body).await
}

async fn post_tenant(
    state: &ServerState,
    tenant: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("caller", "test", None),
        ));
    let response = temper_server::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

// ── RED #1: duplicate explicit-id POST must be 409, not 201 ──────────────

#[tokio::test]
async fn duplicate_explicit_id_post_is_conflict_governed_actor_path() {
    let store = SimEventStore::no_faults(52901);
    let writer = governed_fixture(store.clone(), "default", "dup-governed-writer");
    let (status, body) = post(&writer, "/tdata/Orders", json!({"Id":"dup-1"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = post(
        &writer,
        "/tdata/Orders",
        json!({"Id":"dup-1", "Notes":"forged"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "EntityAlreadyExists");

    let events = store.read_events("default:Order:dup-1", 0).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "duplicate POST must not append a second Created event: {events:?}"
    );
    assert_eq!(events[0].event_type, "Created");
}

#[tokio::test]
async fn duplicate_explicit_id_post_is_conflict_data_only_path_cold() {
    let store = SimEventStore::no_faults(52902);
    let writer = data_only_fixture(store.clone(), "default", "dup-data-only-writer");
    let (status, body) = post(&writer, "/tdata/Orders", json!({"Id":"dup-2"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        writer.active_actor_count(),
        0,
        "fast path must not spawn an actor"
    );

    // Cold: a brand new ServerState re-reading the same durable journal.
    let reader = data_only_fixture(store.clone(), "default", "dup-data-only-reader");
    let (status, body) = post(&reader, "/tdata/Orders", json!({"Id":"dup-2"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "EntityAlreadyExists");

    let events = store.read_events("default:Order:dup-2", 0).await.unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
}

// ── RED #2: POST to a tombstoned id must be 409, not a zombie 201 ────────

#[tokio::test]
async fn post_to_deleted_id_is_conflict_not_resurrection_governed_actor_path() {
    let store = SimEventStore::no_faults(52903);
    let writer = governed_fixture(store.clone(), "default", "del-governed-writer");
    let (status, body) = post(&writer, "/tdata/Orders", json!({"Id":"del-1"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    writer
        .delete_tenant_entity(&TenantId::default(), "Order", "del-1")
        .await
        .expect("delete succeeds");

    let (status, body) = post(
        &writer,
        "/tdata/Orders",
        json!({"Id":"del-1", "Notes":"should not resurrect"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "EntityAlreadyExists");

    let events = store.read_events("default:Order:del-1", 0).await.unwrap();
    assert_eq!(
        events.len(),
        2,
        "re-POST must not append anything past the tombstone: {events:?}"
    );
    assert_eq!(events[0].event_type, "Created");
    assert_eq!(events[1].event_type, "Deleted");
    assert!(
        !writer.entity_exists(&TenantId::default(), "Order", "del-1"),
        "a tombstoned id must not reappear live"
    );
}

#[tokio::test]
async fn post_to_deleted_id_is_conflict_not_resurrection_cold_reader() {
    let store = SimEventStore::no_faults(52904);
    let writer = governed_fixture(store.clone(), "default", "del-cold-writer");
    assert_eq!(
        post(&writer, "/tdata/Orders", json!({"Id":"del-2"}))
            .await
            .0,
        StatusCode::CREATED
    );
    writer
        .delete_tenant_entity(&TenantId::default(), "Order", "del-2")
        .await
        .expect("delete succeeds");

    // A brand new process-equivalent ServerState, with no in-memory index
    // or actor registry entry for this id at all, must still see the
    // tombstone as authoritative history and refuse to resurrect it.
    let reader = governed_fixture(store.clone(), "default", "del-cold-reader");
    assert_eq!(reader.active_actor_count(), 0);
    let (status, body) = post(&reader, "/tdata/Orders", json!({"Id":"del-2"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "EntityAlreadyExists");
    assert_eq!(reader.active_actor_count(), 0, "no actor must be spawned");

    let events = store.read_events("default:Order:del-2", 0).await.unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].event_type, "Deleted");
}

// ── RED #3: concurrent duplicate POST -> exactly one 201, one 409 ────────

#[tokio::test]
async fn concurrent_duplicate_post_new_id_governed_path_yields_one_created_one_conflict() {
    for seed in [52910u64, 52911, 52912, 52913] {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = governed_fixture(store.clone(), "default", &format!("race-gov-{seed}"));
        let persistence_id = "default:Order:race-new";

        let (a, b) = tokio::join!(
            post(&state, "/tdata/Orders", json!({"Id":"race-new"})),
            post(&state, "/tdata/Orders", json!({"Id":"race-new"})),
        );
        let statuses = [a.0, b.0];
        let created = statuses
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let conflict = statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count();
        assert_eq!(
            (created, conflict),
            (1, 1),
            "seed={seed}: a={:?} {} b={:?} {}",
            a.0,
            a.1,
            b.0,
            b.1
        );

        let events = store.read_events(persistence_id, 0).await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "seed={seed}: exactly one Created event, got {events:?}"
        );
        assert_eq!(events[0].event_type, "Created");
    }
}

#[tokio::test]
async fn concurrent_duplicate_post_new_id_data_only_path_yields_one_created_one_conflict() {
    for seed in [52920u64, 52921, 52922] {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = data_only_fixture(store.clone(), "default", &format!("race-data-{seed}"));
        let persistence_id = "default:Order:race-data-new";
        // Force a genuine interleaving instead of two sequential completions:
        // delay whichever caller's append reaches the store first so the
        // other caller's own zero-history append also lands, and the
        // original caller then loses its own compare-and-append once it
        // wakes -- the conflict must be decided at that CAS, not by a
        // preflight read that happened to run first.
        store.inject_append_delay(persistence_id, Duration::from_millis(15));

        let (a, b) = tokio::join!(
            post(&state, "/tdata/Orders", json!({"Id":"race-data-new"})),
            post(&state, "/tdata/Orders", json!({"Id":"race-data-new"})),
        );
        let statuses = [a.0, b.0];
        let created = statuses
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let conflict = statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count();
        assert_eq!(
            (created, conflict),
            (1, 1),
            "seed={seed}: a={:?} {} b={:?} {}",
            a.0,
            a.1,
            b.0,
            b.1
        );

        let events = store.read_events(persistence_id, 0).await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "seed={seed}: exactly one Created event, got {events:?}"
        );
        assert_eq!(events[0].event_type, "Created");
    }
}

// ── Controls ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn brand_new_id_still_creates_governed_and_data_only() {
    let store = SimEventStore::no_faults(52930);
    let governed = governed_fixture(store.clone(), "default", "control-governed");
    let (status, body) = post(&governed, "/tdata/Orders", json!({"Id":"fresh-governed"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let store = SimEventStore::no_faults(52931);
    let data_only = data_only_fixture(store.clone(), "default", "control-data-only");
    let (status, body) = post(&data_only, "/tdata/Orders", json!({"Id":"fresh-data-only"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn same_id_different_tenants_are_independent() {
    let store = SimEventStore::no_faults(52940);
    let state_a = governed_fixture(store.clone(), "acme", "tenant-isolation-a");
    let state_b = governed_fixture(store.clone(), "beta", "tenant-isolation-b");

    let (status, body) =
        post_tenant(&state_a, "acme", "/tdata/Orders", json!({"Id":"shared-id"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) =
        post_tenant(&state_b, "beta", "/tdata/Orders", json!({"Id":"shared-id"})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a different tenant's history must not conflict: {body}"
    );

    // Each tenant creating a SECOND time against the same id still conflicts
    // within its own tenant, proving isolation is per-tenant, not global.
    let (status, _) =
        post_tenant(&state_a, "acme", "/tdata/Orders", json!({"Id":"shared-id"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) =
        post_tenant(&state_b, "beta", "/tdata/Orders", json!({"Id":"shared-id"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn internal_get_or_create_caller_is_unaffected_by_create_only() {
    let store = SimEventStore::no_faults(52950);
    let state = governed_fixture(store.clone(), "default", "internal-caller-unaffected");

    // Bootstrap/file/stream-init-style internal callers use
    // `get_or_create_tenant_entity` directly and must keep succeeding (not
    // 409-ing) on an id that already has history, same as before V11.
    let first = state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Order",
            "internal-1",
            json!({"Id":"internal-1"}),
        )
        .await
        .expect("first internal get-or-create succeeds");
    assert!(first.success);

    let second = state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Order",
            "internal-1",
            json!({"Id":"internal-1"}),
        )
        .await
        .expect("internal get-or-create on an existing id stays idempotent-success, not 409");
    assert!(second.success);

    let events = store
        .read_events("default:Order:internal-1", 0)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "internal get-or-create must not append a second Created either: {events:?}"
    );
}

#[tokio::test]
async fn backend_read_failure_is_server_error_not_conflict_or_create() {
    let store = SimEventStore::no_faults(52960);
    let state = governed_fixture(store.clone(), "default", "backend-fault");
    let persistence_id = "default:Order:fault-1";
    store.fail_next_reads(persistence_id, 1);

    let (status, body) = post(&state, "/tdata/Orders", json!({"Id":"fault-1"})).await;
    assert!(
        status.is_server_error(),
        "a storage read failure must surface as a server error, not 409 or 201: {status} {body}"
    );
    assert_ne!(status, StatusCode::CONFLICT);
    assert_ne!(status, StatusCode::CREATED);

    let events = store.read_events(persistence_id, 0).await.unwrap();
    assert!(
        events.is_empty(),
        "a failed create attempt must never leave a false Created event: {events:?}"
    );
}
