use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::{EntityMsg, ServerState, SpecRegistry, StorageStack, build_router};

const TENANT: &str = "arn216";
const NOTE_V1: &str = include_str!("../../tests/fixtures/arn216/full_v1/note.ioa.toml");
const NOTE_V2: &str = include_str!("../../tests/fixtures/arn216/full_v2/note.ioa.toml");
const CSDL_V2: &str = include_str!("../../tests/fixtures/arn216/full_v2/model.csdl.xml");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/arn216")
        .join(name)
}

fn sim_state(store: &SimEventStore, name: &str) -> ServerState {
    let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}

fn turso_state(store: &TursoEventStore, name: &str) -> ServerState {
    let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
}

async fn load_dir(state: &ServerState, fixture_name: &str) {
    let body = serde_json::json!({
        "tenant": TENANT,
        "specs_dir": fixture(fixture_name),
        "merge": false,
    });
    let response = build_router(state.clone())
        .oneshot(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build load-dir request"),
        )
        .await
        .expect("call load-dir");
    assert_eq!(response.status(), StatusCode::OK);
}

fn envelope(actor_id: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: actor_id.to_string(),
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sim_load_dir_restart_tombstones_durable_only_omissions_and_readds() {
    let (_guard, _clock, _ids) = install_deterministic_context(216);
    let store = SimEventStore::no_faults(216);
    let first = sim_state(&store, "arn216-sim-first");
    load_dir(&first, "full_v1").await;
    assert_eq!(
        store
            .spec_declaration_entity_types(TENANT)
            .await
            .expect("present declarations"),
        vec!["Item".to_string(), "Note".to_string()]
    );

    drop(first);
    let restarted = sim_state(&store, "arn216-sim-restarted");
    load_dir(&restarted, "item_only").await;
    assert_eq!(
        store
            .spec_declaration_entity_types(TENANT)
            .await
            .expect("post-replacement declarations"),
        vec!["Item".to_string()],
        "the restarted registry must tombstone durable-only Note authority"
    );

    let stale_v1 = temper_store_turso::spec_content_hash(NOTE_V1);
    let stale = store
        .append_with_index_rows(
            &format!("{TENANT}:Note:stale-v1"),
            0,
            &[envelope("stale-v1")],
            &[],
            &[],
            false,
            Some(&stale_v1),
        )
        .await
        .expect_err("omitted Note writer must be fenced");
    assert!(
        stale
            .to_string()
            .contains("stale live vector declaration fingerprint")
    );

    load_dir(&restarted, "full_v2").await;
    let fingerprint_v2 = temper_store_turso::spec_content_hash(NOTE_V2);
    store
        .append_with_index_rows(
            &format!("{TENANT}:Note:current-v2"),
            0,
            &[envelope("current-v2")],
            &[],
            &[],
            false,
            Some(&fingerprint_v2),
        )
        .await
        .expect("re-added Note v2 writer");
    assert!(
        store
            .append_with_index_rows(
                &format!("{TENANT}:Note:stale-after-readd"),
                0,
                &[envelope("stale-after-readd")],
                &[],
                &[],
                false,
                Some(&stale_v1),
            )
            .await
            .is_err(),
        "identical type re-add with changed source must retain monotonic authority"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn turso_load_dir_commits_scoped_replacement_across_restart() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-arn216-load-dir-{}.db",
        uuid::Uuid::new_v4()
    ));
    let url = format!("file:{}", db_path.display());
    let first_store = TursoEventStore::new(&url, None).await.expect("open Turso");
    let first = turso_state(&first_store, "arn216-turso-first");
    load_dir(&first, "full_v1").await;
    drop(first);
    drop(first_store);

    let second_store = TursoEventStore::new(&url, None)
        .await
        .expect("reopen Turso");
    let second = turso_state(&second_store, "arn216-turso-second");
    load_dir(&second, "item_only").await;
    drop(second);
    drop(second_store);

    let third_store = TursoEventStore::new(&url, None)
        .await
        .expect("reopen replaced catalog");
    let specs = third_store
        .load_specs()
        .await
        .expect("load committed specs");
    let types = specs
        .iter()
        .filter(|row| row.tenant == TENANT)
        .map(|row| row.entity_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(types, vec!["Item"]);
    assert!(
        specs
            .iter()
            .filter(|row| row.tenant == TENANT)
            .all(|row| row.committed)
    );

    let third = turso_state(&third_store, "arn216-turso-third");
    load_dir(&third, "full_v2").await;
    drop(third);
    drop(third_store);
    let final_store = TursoEventStore::new(&url, None)
        .await
        .expect("reopen re-added catalog");
    let note = final_store
        .load_specs()
        .await
        .expect("load re-added specs")
        .into_iter()
        .find(|row| row.tenant == TENANT && row.entity_type == "Note")
        .expect("committed Note v2");
    assert!(note.committed);
    assert_eq!(
        note.content_hash.as_deref(),
        Some(temper_store_turso::spec_content_hash(NOTE_V2).as_str())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn existing_actor_hot_swaps_in_place_and_removed_actor_stops() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-arn216-publication-{}.db",
        uuid::Uuid::new_v4()
    ));
    let url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&url, None).await.expect("open Turso");
    let state = turso_state(&store, "arn216-publication-gap");
    load_dir(&state, "full_v1").await;
    let tenant = TenantId::from(TENANT);
    let existing_actor = state
        .get_or_spawn_tenant_actor(&tenant, "Note", "existing-note")
        .expect("spawn Note v1 before durable replacement");
    existing_actor
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("pre-existing actor must finish v1 startup");

    load_dir(&state, "full_v2").await;
    existing_actor
        .ask::<crate::EntityResponse>(
            EntityMsg::Action {
                name: "Review".to_string(),
                params: serde_json::json!({"Body": "survives hot swap"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("pre-existing actor must survive and execute Note v2 Review");

    load_dir(&state, "item_only").await;
    assert!(
        existing_actor
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "an actor whose type is omitted must be stopped"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn publication_snapshot_evicts_unready_restarted_and_same_key_replacements() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-arn216-actor-identity-{}.db",
        uuid::Uuid::new_v4()
    ));
    let url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&url, None).await.expect("open Turso");
    let state = turso_state(&store, "arn216-actor-identity");
    load_dir(&state, "full_v1").await;
    let tenant = TenantId::from(TENANT);
    let note_types = vec!["Note".to_string()];

    let unready = state
        .get_or_spawn_tenant_actor(&tenant, "Note", "unready-note")
        .expect("spawn actor without yielding to pre_start");
    let empty_snapshot = state.ready_actor_identities_for_types(&tenant, &note_types);
    assert!(
        empty_snapshot.is_empty(),
        "an ActorRef inserted before pre_start must not be preserved"
    );
    state.evict_type_actors_except(&tenant, &note_types, &empty_snapshot);
    assert!(
        unready
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "an unready publication-gap actor must be evicted"
    );

    let original = state
        .get_or_spawn_tenant_actor(&tenant, "Note", "same-key")
        .expect("spawn original same-key actor");
    original
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("original actor must become ready");
    let original_snapshot = state.ready_actor_identities_for_types(&tenant, &note_types);
    let original_incarnation = original
        .ready_incarnation()
        .expect("ready actor must expose its supervised incarnation");
    assert_eq!(
        original_snapshot.get(&format!("{TENANT}:Note:same-key")),
        Some(&(original.id().uid, original_incarnation))
    );

    original
        .signal(temper_runtime::actor::SystemSignal::Restart)
        .expect("request supervised restart");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if original
                .ready_incarnation()
                .is_some_and(|incarnation| incarnation != original_incarnation)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor must complete a new supervised incarnation");
    assert_eq!(
        original.id().uid,
        original_snapshot[&format!("{TENANT}:Note:same-key")].0
    );
    state.evict_type_actors_except(&tenant, &note_types, &original_snapshot);
    assert!(
        original
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "a supervised restart must not inherit preservation from its prior incarnation"
    );

    let replacement_key = "same-key-replacement";
    let original = state
        .get_or_spawn_tenant_actor(&tenant, "Note", replacement_key)
        .expect("spawn original actor for same-key replacement");
    original
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("original replacement-test actor must become ready");
    let original_snapshot = state.ready_actor_identities_for_types(&tenant, &note_types);
    state.stop_and_remove_entity(&tenant, "Note", replacement_key);
    let replacement = state
        .get_or_spawn_tenant_actor(&tenant, "Note", replacement_key)
        .expect("spawn same-key replacement");
    assert_ne!(replacement.id().uid, original.id().uid);
    state.evict_type_actors_except(&tenant, &note_types, &original_snapshot);
    assert!(
        replacement
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "a same-key actor with a different uid must not inherit preservation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn publication_rechecks_supervised_restart_after_initial_eviction() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-arn216-post-eviction-restart-{}.db",
        uuid::Uuid::new_v4()
    ));
    let url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&url, None).await.expect("open Turso");
    let state = turso_state(&store, "arn216-post-eviction-restart");
    load_dir(&state, "full_v1").await;
    let tenant = TenantId::from(TENANT);
    let note_types = vec!["Note".to_string()];
    let original = state
        .get_or_spawn_tenant_actor(&tenant, "Note", "restart-window")
        .expect("spawn original actor");
    original
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("original actor must become ready");
    let preserved = state.ready_actor_identities_for_types(&tenant, &note_types);
    let original_incarnation = original
        .ready_incarnation()
        .expect("original incarnation must be ready");

    state.evict_type_actors_except(&tenant, &note_types, &preserved);
    original
        .signal(temper_runtime::actor::SystemSignal::Restart)
        .expect("restart between initial eviction and registry swap");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if original
                .ready_incarnation()
                .is_some_and(|incarnation| incarnation != original_incarnation)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restart must complete before registry publication");

    state
        .registry
        .write()
        .expect("registry lock")
        .try_register_tenant(
            TENANT,
            temper_spec::csdl::parse_csdl(CSDL_V2).expect("parse v2 CSDL"),
            CSDL_V2.to_string(),
            &[("Note", NOTE_V2)],
        )
        .expect("publish Note v2");
    state.revalidate_type_actors_after_publication(&tenant, &note_types, &preserved);

    assert!(
        original
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "an actor restarted after initial eviction must not survive the registry swap with its old cloned table"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn first_registry_publication_evicts_legacy_fallback_actor() {
    let (_guard, _clock, _ids) = install_deterministic_context(219);
    let store = SimEventStore::no_faults(219);
    let mut state = sim_state(&store, "arn216-first-publication");
    state.transition_tables = std::sync::Arc::new(BTreeMap::from([(
        "Note".to_string(),
        std::sync::Arc::new(temper_jit::table::TransitionTable::from_ioa_source(NOTE_V1)),
    )]));
    let tenant = TenantId::from(TENANT);
    let legacy = state
        .get_or_spawn_tenant_actor(&tenant, "Note", "legacy-note")
        .expect("legacy fallback must govern Note before first publication");
    legacy
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("legacy actor must become ready");

    load_dir(&state, "full_v2").await;
    assert!(
        legacy
            .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_millis(100))
            .await
            .is_err(),
        "first registry publication must evict actors holding cloned fallback tables"
    );
    state
        .get_or_spawn_tenant_actor(&tenant, "Note", "legacy-note")
        .expect("published Note v2 must spawn a registry-backed actor")
        .ask::<crate::EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("registry-backed replacement must be live");
}
