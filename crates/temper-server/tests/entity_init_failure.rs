//! An entity create that dies inside `pre_start` must be diagnosable.
//!
//! Reproduces the production chain that turned a single declared-key collision
//! into a 25-minute outage: `POST /tdata/<set>` spawns a fresh `EntityActor`,
//! `pre_start` co-commits the bootstrap `Created` event, the declared-key
//! uniqueness check rejects it, the cell burns its restarts and returns —
//! dropping every waiting reply channel. The caller saw
//! `500 {"code":"CreateError","message":"Actor query failed: actor stopped"}`
//! and retried ~240 times because nothing told it the truth.
//!
//! What must hold now:
//! 1. the waiting `ask` gets the *cause*, classified, not a bare "actor stopped";
//! 2. the HTTP layer answers 409 naming the key and its holder, never a 500
//!    with the cause discarded;
//! 3. the failed spawn is retracted, so the retry re-derives the same honest
//!    answer instead of "send failed: actor not running";
//! 4. the failure is counted per entity type and visible on `/observe/metrics`,
//!    which a create that dies before its first event never reached.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::InitFailureKind;
use temper_runtime::persistence::{EntityKeyRow, EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::state::EntityCreateError;
use temper_server::storage::{BackendLabel, BoxedEventStore, StorageStack};
use temper_server::{EntityActor, EntityMsg, ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};
use tower::ServiceExt;

const DOC_IOA: &str = include_str!("../../../test-fixtures/specs/keyed_doc.ioa.toml");

const DOC_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.EntityInitFailureTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Doc">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="WorkspaceId" Type="Edm.String"/>
        <Property Name="Path" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Docs" EntityType="Temper.EntityInitFailureTest.Doc"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn doc_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)))
}

fn test_envelope() -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "Created".to_string(),
        payload: serde_json::json!({ "action": "Created" }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "default:Doc:holder".to_string(),
        },
    }
}

/// Give `key_name` to `holder_id` so the next writer collides with it.
async fn claim_key(store: &SimEventStore, holder_id: &str, key_hash: &str) {
    store
        .append_with_keys(
            &format!("default:Doc:{holder_id}"),
            0,
            &[test_envelope()],
            &[EntityKeyRow {
                key_name: "path".to_string(),
                key_hash: key_hash.to_string(),
            }],
        )
        .await
        .expect("the first writer claims the key");
}

/// A store whose every append fails — the shape of a backend outage, as
/// opposed to a write the backend deliberately rejected.
fn store_that_always_fails_writes(seed: u64) -> SimEventStore {
    SimEventStore::new(
        seed,
        SimFaultConfig {
            write_failure_prob: 1.0,
            ..SimFaultConfig::none()
        },
    )
}

fn doc_key_hash(workspace: &str, path: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("WorkspaceId".to_string(), serde_json::json!(workspace));
    fields.insert("Path".to_string(), serde_json::json!(path));
    temper_server::key_index::canonical_key_hash(
        "path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &fields,
    )
    .expect("both key properties are present")
}

/// The actor boundary: a `pre_start` that cannot persist its bootstrap event
/// must answer the waiting caller with the classified cause.
#[tokio::test]
async fn ask_against_a_failed_init_carries_the_cause() {
    let (_guard, _clock, _id) = install_deterministic_context(7);
    let store = SimEventStore::no_faults(7);
    claim_key(&store, "doc-holder", &doc_key_hash("ws1", "/a.md")).await;
    let boxed: BoxedEventStore = BoxedEventStore::new(store);

    let system = ActorSystem::new("init-failure");
    let actor = EntityActor::with_persistence(
        "Doc",
        "doc-colliding",
        doc_table(),
        serde_json::json!({ "WorkspaceId": "ws1", "Path": "/a.md" }),
        boxed,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "doc-colliding");

    let err = actor_ref
        .ask::<temper_server::EntityResponse>(EntityMsg::GetState, Duration::from_secs(10))
        .await
        .expect_err("the bootstrap append is rejected, so init cannot succeed");

    assert_eq!(
        err.init_failure_kind(),
        Some(InitFailureKind::Constraint),
        "a declared-key rejection is a constraint failure, got: {err}"
    );
    assert!(
        err.is_permanent(),
        "retrying the same colliding key can never succeed, got: {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("duplicate declared key 'path'") && message.contains("doc-holder"),
        "the caller must be told which key collided and who holds it, got: {message}"
    );
    assert!(
        !message.contains("actor stopped"),
        "the cause must not degrade to 'actor stopped', got: {message}"
    );
}

/// A `pre_start` failure that is *not* the caller's fault stays retryable, so
/// the dispatch layer and the HTTP layer can back off instead of giving up.
#[tokio::test]
async fn a_dependency_failure_during_init_stays_transient() {
    let (_guard, _clock, _id) = install_deterministic_context(11);
    let store = store_that_always_fails_writes(11);
    let boxed: BoxedEventStore = BoxedEventStore::new(store);

    let system = ActorSystem::new("init-transient");
    let actor = EntityActor::with_persistence(
        "Doc",
        "doc-unlucky",
        doc_table(),
        serde_json::json!({ "WorkspaceId": "ws1", "Path": "/b.md" }),
        boxed,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "doc-unlucky");

    let err = actor_ref
        .ask::<temper_server::EntityResponse>(EntityMsg::GetState, Duration::from_secs(10))
        .await
        .expect_err("every append fails, so init cannot succeed");

    assert_eq!(
        err.init_failure_kind(),
        Some(InitFailureKind::TransientDependency),
        "a store fault is not the caller's fault, got: {err}"
    );
    assert!(
        err.is_transient(),
        "a store fault must be retryable so ADR-0048 retry applies, got: {err}"
    );
}

fn build_doc_state(name: &str, store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(DOC_CSDL_XML).expect("Doc CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        DOC_CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    {
        let mut registry = state.registry.write().unwrap();
        registry.set_verification_status(
            &TenantId::default(),
            "Doc",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0".to_string(),
                    passed: true,
                    summary: "test fixture verified".to_string(),
                    details: None,
                }],
                verified_at: "2026-08-06T00:00:00Z".to_string(),
            }),
        );
    }
    state
}

/// The server boundary: a create that collides on a declared key is reported as
/// a conflict, and the failed spawn leaves no corpse behind — the identical
/// retry gets the identical answer, not "send failed: actor not running".
#[tokio::test]
async fn colliding_create_is_a_conflict_and_stays_one_on_retry() {
    let (_guard, _clock, _id) = install_deterministic_context(13);
    let store = SimEventStore::no_faults(13);
    claim_key(&store, "doc-holder", &doc_key_hash("ws1", "/a.md")).await;
    let state = build_doc_state("init-failure-conflict", store);
    let tenant = TenantId::default();
    let fields = serde_json::json!({ "WorkspaceId": "ws1", "Path": "/a.md" });

    for attempt in 1..=3 {
        let err = state
            .get_or_create_tenant_entity(&tenant, "Doc", "doc-colliding", fields.clone())
            .await
            .expect_err("attempt {attempt}: the declared key is already held");
        match &err {
            EntityCreateError::DeclaredKeyConflict { detail } => {
                assert!(
                    detail.contains("duplicate declared key 'path'")
                        && detail.contains("doc-holder"),
                    "attempt {attempt}: the conflict must name the key and its holder, got: {detail}"
                );
            }
            other => panic!(
                "attempt {attempt}: expected a declared-key conflict, got {other:?} ({other})"
            ),
        }
    }

    // Nothing exists, so nothing may be listed: the optimistic index entry
    // written at spawn time must have been retracted with the actor.
    let index = state.entity_index.read().unwrap();
    let listed = index
        .get("default:Doc")
        .map(|ids| ids.contains("doc-colliding"))
        .unwrap_or(false);
    assert!(
        !listed,
        "an entity that never persisted must not appear in the entity index"
    );
    drop(index);

    let registry = state.actor_registry.read().unwrap();
    assert!(
        !registry.contains_key("default:Doc:doc-colliding"),
        "a dead actor must not stay in the registry for the next request to ask"
    );
}

/// The wire contract: 409 with the key and the holder, not 500 with the cause
/// flattened into a sentence.
#[tokio::test]
async fn post_returns_409_naming_the_key_and_the_holder() {
    let (_guard, _clock, _id) = install_deterministic_context(17);
    let store = SimEventStore::no_faults(17);
    claim_key(&store, "doc-holder", &doc_key_hash("ws1", "/a.md")).await;
    let state = build_doc_state("init-failure-http", store);

    let router = build_router(state.clone());
    let response = router
        .oneshot(
            Request::post("/tdata/Docs")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"Id":"doc-colliding","WorkspaceId":"ws1","Path":"/a.md"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a declared-key collision is the caller's to fix, got {status} with body {body}"
    );
    assert_eq!(body["error"]["code"], "DeclaredKeyConflict");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("duplicate declared key 'path'") && message.contains("doc-holder"),
        "the 409 must name the key and its holder, got: {message}"
    );

    // The failure is now visible per entity type — a create that dies before
    // its first event writes no trajectory row, so this counter is the only
    // place a fully broken entity type shows up. (The `/observe/metrics`
    // exposition of the same counter is covered in `observe::mod_test`, which
    // runs under the `observe` feature.)
    assert_eq!(
        spawn_failures(&state, "Doc", "declared_key_conflict"),
        1,
        "the failed create must be counted for its entity type"
    );
}

/// A store outage during create answers 503 with `Retry-After`, so a client
/// backs off instead of retrying in a tight loop.
#[tokio::test]
async fn post_returns_503_with_retry_after_when_the_store_is_down() {
    let (_guard, _clock, _id) = install_deterministic_context(19);
    let state = build_doc_state(
        "init-failure-unavailable",
        store_that_always_fails_writes(19),
    );

    let router = build_router(state.clone());
    let response = router
        .oneshot(
            Request::post("/tdata/Docs")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"Id":"doc-unlucky","WorkspaceId":"ws1","Path":"/b.md"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a store fault is ours, and it may clear — say so"
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a retryable failure must tell the caller when to come back"
    );

    assert_eq!(
        spawn_failures(&state, "Doc", "transient_dependency"),
        1,
        "a dependency outage must be counted apart from a caller error"
    );
    assert_eq!(
        spawn_failures(&state, "Doc", "declared_key_conflict"),
        0,
        "a store outage must not be filed as the caller's mistake"
    );

    // The dead actor is retracted so the retry respawns instead of asking a
    // corpse — but the entity is NOT un-listed. A store fault says nothing
    // about whether the entity exists, and un-listing on that basis would hide
    // a real entity that merely failed to hydrate.
    let registry = state.actor_registry.read().unwrap();
    assert!(
        !registry.contains_key("default:Doc:doc-unlucky"),
        "a dead actor must not be left for the next request"
    );
    drop(registry);
    let index = state.entity_index.read().unwrap();
    assert!(
        index
            .get("default:Doc")
            .map(|ids| ids.contains("doc-unlucky"))
            .unwrap_or(false),
        "an unproven absence must not un-list the entity"
    );
}

/// Nobody is waiting. The request that would have received the failure was
/// abandoned, or never asked at all — spawning is the first thing a read does,
/// and the caller can be gone before `pre_start` even finishes. A retraction
/// that only fires when someone receives the error leaves this corpse behind
/// forever, and every later request for the entity asks its dead mailbox.
#[tokio::test]
async fn an_init_failure_nobody_waits_for_is_still_retracted() {
    let (_guard, _clock, _id) = install_deterministic_context(23);
    let store = SimEventStore::no_faults(23);
    claim_key(&store, "doc-holder", &doc_key_hash("ws1", "/a.md")).await;
    let state = build_doc_state("init-failure-unwatched", store);
    let tenant = TenantId::default();

    // Spawn and walk away: no ask, no dispatch, no HTTP request.
    let actor_ref = state
        .get_or_spawn_tenant_actor_with_fields(
            &tenant,
            "Doc",
            "doc-colliding",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/a.md" }),
        )
        .expect("the Doc entity type has a transition table");
    assert!(
        registered(&state, "default:Doc:doc-colliding"),
        "the spawn is registered optimistically — that is what has to be undone"
    );
    drop(actor_ref);

    await_true("the unwatched failed spawn was never retracted", || {
        !registered(&state, "default:Doc:doc-colliding")
    })
    .await;
    assert!(
        !listed(&state, "default:Doc", "doc-colliding"),
        "a constraint failure proves the entity does not exist, so it must not \
         stay listed either — whether or not anyone was waiting to be told"
    );
}

/// The invariant is not the create path's alone. A PATCH against a cold entity
/// spawns it too, so an update can be the request that discovers a broken init
/// — and must leave no corpse for the next one to ask.
#[tokio::test]
async fn a_field_update_that_breaks_init_leaves_no_corpse() {
    let (_guard, _clock, _id) = install_deterministic_context(29);
    let state = build_doc_state("init-failure-patch", store_that_always_fails_writes(29));
    let tenant = TenantId::default();

    let err = state
        .update_tenant_entity_fields(
            &tenant,
            "Doc",
            "doc-patched",
            serde_json::json!({ "Status": "Draft" }),
            false,
        )
        .await
        .expect_err("every append fails, so the actor never starts");
    assert!(
        err.contains("Actor update failed"),
        "the update must report the actor failure, got: {err}"
    );

    assert!(
        !registered(&state, "default:Doc:doc-patched"),
        "a dead actor must not be left for the next PATCH to ask"
    );
    assert!(
        listed(&state, "default:Doc", "doc-patched"),
        "a store fault proves nothing about existence, so the entity stays listed"
    );
}

/// Same for DELETE: it spawns the entity to tombstone it, and a spawn that dies
/// in `pre_start` must not survive the request that provoked it.
#[tokio::test]
async fn a_delete_that_breaks_init_leaves_no_corpse() {
    let (_guard, _clock, _id) = install_deterministic_context(31);
    let state = build_doc_state("init-failure-delete", store_that_always_fails_writes(31));
    let tenant = TenantId::default();

    let err = state
        .delete_tenant_entity(&tenant, "Doc", "doc-deleted")
        .await
        .expect_err("every append fails, so the actor never starts");
    assert!(
        err.contains("Actor delete failed"),
        "the delete must report the actor failure, got: {err}"
    );

    assert!(
        listed(&state, "default:Doc", "doc-deleted"),
        "the entity was listed by the spawn this test depends on"
    );
    assert!(
        !registered(&state, "default:Doc:doc-deleted"),
        "a dead actor must not be left for the next DELETE to ask"
    );
}

/// A dispatched action takes the same path — the generic mutation verb, not
/// just the named create.
#[tokio::test]
async fn a_dispatched_action_that_breaks_init_leaves_no_corpse() {
    let (_guard, _clock, _id) = install_deterministic_context(37);
    let state = build_doc_state("init-failure-dispatch", store_that_always_fails_writes(37));
    let tenant = TenantId::default();

    let outcome = state
        .dispatch_tenant_action(
            &tenant,
            "Doc",
            "doc-dispatched",
            "Create",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/c.md" }),
            &AgentContext::default(),
        )
        .await;
    assert!(
        outcome.is_err(),
        "every append fails, so the action cannot be applied"
    );

    // Only a spawn lists the entity, so this also proves the dispatch got past
    // its governance gates and really did reach the actor.
    assert!(
        listed(&state, "default:Doc", "doc-dispatched"),
        "the dispatch must have spawned the entity for this test to mean anything"
    );
    assert!(
        !registered(&state, "default:Doc:doc-dispatched"),
        "a dead actor must not be left for the next dispatch to ask"
    );
}

fn registered(state: &ServerState, actor_key: &str) -> bool {
    state.actor_registry.read().unwrap().contains_key(actor_key)
}

fn listed(state: &ServerState, index_key: &str, entity_id: &str) -> bool {
    state
        .entity_index
        .read()
        .unwrap()
        .get(index_key)
        .is_some_and(|ids| ids.contains(entity_id))
}

/// Wait for a condition the failed actor's own task brings about. Bounded, so a
/// regression fails the suite instead of hanging it.
async fn await_true(what_failed: &str, cond: impl Fn() -> bool) {
    for _ in 0..600 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what_failed} (waited 6s)");
}

/// How many creates of `entity_type` failed for `reason`, as `/observe/metrics`
/// would report it.
fn spawn_failures(state: &ServerState, entity_type: &str, reason: &str) -> u64 {
    state
        .metrics
        .entity_spawn_failures
        .read()
        .unwrap()
        .get(&format!("{entity_type}:{reason}"))
        .copied()
        .unwrap_or(0)
}

/// Guard against a silently reintroduced `BTreeMap` import churn: the fixture
/// spec must actually declare the key this suite depends on.
#[test]
fn fixture_declares_the_key_under_test() {
    let table = TransitionTable::from_ioa_source(DOC_IOA);
    let declared: BTreeMap<&str, Vec<String>> = table
        .keys
        .iter()
        .map(|k| (k.name.as_str(), k.properties.clone()))
        .collect();
    assert_eq!(
        declared.get("path"),
        Some(&vec!["WorkspaceId".to_string(), "Path".to_string()]),
        "the suite reproduces a collision on the declared (WorkspaceId, Path) key"
    );
}
