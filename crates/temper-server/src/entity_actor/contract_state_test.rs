use super::*;
use std::time::Duration;
use temper_runtime::ActorSystem;

const OVERFLOW_CONTRACT: &str = r#"
[automaton]
name = "Document"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "Name"
type = "string"
initial = "initial"
[[action]]
name = "Write"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Name"]
[[action]]
name = "Same"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["expected"]
constraints = [{kind="param_equals_field", param="expected", field="Name"}]
[[action]]
name = "Different"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["expected"]
constraints = [{kind="param_not_equals_field", param="expected", field="Name"}]
"#;

fn overflow_action(name: &str, params: serde_json::Value) -> EntityMsg {
    EntityMsg::Action {
        name: name.into(),
        params,
        cross_entity_booleans: BTreeMap::new(),
        idempotency_key: None,
        expected_authorization_precondition: None,
    }
}

#[tokio::test]
async fn round_three_blob_comparisons_read_the_logical_value_without_mutating_refs() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::blob_store::BlobStore::local_fs(dir.path());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        OVERFLOW_CONTRACT,
    )));
    let system = ActorSystem::new("overflow-comparison");
    let journal = Arc::new(
        temper_store_turso::TursoEventStore::new(
            dir.path().join("events.db").to_str().unwrap(),
            None,
        )
        .await
        .unwrap(),
    );
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Document",
            "blob",
            table.clone(),
            serde_json::json!({}),
            crate::storage::BoxedEventStore::from_arc(journal.clone()),
            crate::storage::BackendLabel::Turso,
        )
        .with_blob_store(Some(store.clone())),
        "document",
    );
    let large = "N".repeat(512 * 1024);
    let written: EntityResponse = actor
        .ask(
            overflow_action("Write", serde_json::json!({"Name":large})),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(written.success);
    let descriptor = written.state.fields["Name"].clone();
    assert!(crate::blobs::field_overflow_descriptor(&descriptor).is_some());
    for (action, expected, accepts) in [
        ("Same", large.as_str(), true),
        ("Different", large.as_str(), false),
        ("Same", "stale", false),
        ("Different", "stale", true),
    ] {
        let response: EntityResponse = actor
            .ask(
                overflow_action(action, serde_json::json!({"expected":expected})),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(response.success, accepts, "{action}: {:?}", response.error);
        assert_eq!(
            response.state.fields["Name"], descriptor,
            "comparison rewrote storage representation"
        );
    }
    let restarted = ActorSystem::new("overflow-restart");
    let recovered = restarted.spawn(
        EntityActor::with_persistence(
            "Document",
            "blob",
            table,
            serde_json::json!({}),
            crate::storage::BoxedEventStore::from_arc(journal),
            crate::storage::BackendLabel::Turso,
        )
        .with_blob_store(Some(store.clone())),
        "recovered-document",
    );
    let response: EntityResponse = recovered
        .ask(
            overflow_action("Same", serde_json::json!({"expected":large})),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(
        response.success,
        "recovered blob comparison failed: {:?}",
        response.error
    );
    assert_eq!(response.state.fields["Name"], descriptor);
    let key = crate::blobs::field_overflow_descriptor(&descriptor)
        .unwrap()
        .key
        .to_owned();
    tokio::fs::remove_file(dir.path().join(&key)).await.unwrap();
    for action in ["Same", "Different"] {
        let response: EntityResponse = actor
            .ask(
                overflow_action(action, serde_json::json!({"expected":large})),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(
            !response.success,
            "missing blob must not make inequality pass"
        );
        assert_eq!(response.state.fields["Name"], descriptor);
    }
    // A descriptor with real-looking length and hash must not attest forged bytes.
    let forged = serde_json::to_vec(&serde_json::json!("F".repeat(512 * 1024))).unwrap();
    tokio::fs::write(dir.path().join(&key), forged)
        .await
        .unwrap();
    for action in ["Same", "Different"] {
        let response: EntityResponse = actor
            .ask(
                overflow_action(action, serde_json::json!({"expected":large})),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(
            !response.success,
            "unverified bytes must not make either comparison pass"
        );
        assert_eq!(response.state.fields["Name"], descriptor);
    }
}

#[tokio::test]
async fn round_three_inline_truncation_refuses_to_destroy_a_comparison_target() {
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        OVERFLOW_CONTRACT,
    )));
    let system = ActorSystem::new("inline-comparison");
    let actor = system.spawn(
        EntityActor::new("Document", "inline", table, serde_json::json!({})),
        "document",
    );
    let before: EntityResponse = actor
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();
    let response: EntityResponse = actor
        .ask(
            overflow_action("Write", serde_json::json!({"Name":"N".repeat(512 * 1024)})),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(
        !response.success,
        "accepted a write that makes its declared comparison impossible"
    );
    assert_eq!(
        serde_json::to_value(&response.state).unwrap(),
        serde_json::to_value(&before.state).unwrap()
    );
}

#[tokio::test]
async fn round_three_native_constrained_defaults_drive_guards_and_effects() {
    let source = r#"
[automaton]
name = "Resource"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "sequence"
type = "counter"
initial = "3"
[[state]]
name = "enabled"
type = "bool"
initial = "TRUE"
[[state]]
name = "members"
type = "list"
initial = '["first"]'
[[state]]
name = "offset"
type = "integer"
initial = "-3"
[[action]]
name = "Advance"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["expected"]
guard = "sequence >= 3"
effect = [{type="increment",var="sequence"}]
constraints = [{kind="param_equals_field",param="expected",field="sequence"}]
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let system = ActorSystem::new("constrained-defaults");
    let actor = system.spawn(
        EntityActor::new("Resource", "resource", table, serde_json::json!({})),
        "resource",
    );
    let initial: EntityResponse = actor
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(initial.state.counters["sequence"], 3);
    assert!(initial.state.booleans["enabled"]);
    assert_eq!(initial.state.lists["members"], ["first"]);
    assert_eq!(initial.state.fields["offset"], -3);
    let advanced: EntityResponse = actor
        .ask(
            overflow_action("Advance", serde_json::json!({"expected":3})),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(advanced.success, "{:?}", advanced.error);
    assert_eq!(advanced.state.counters["sequence"], 4);
    let stale: EntityResponse = actor
        .ask(
            overflow_action("Advance", serde_json::json!({"expected":3})),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!stale.success);
    assert_eq!(stale.state.counters["sequence"], 4);
}

#[tokio::test]
async fn round_three_seeded_blob_faults_preserve_refused_state() {
    use crate::entity_actor::action_input::process_action_with_blob_prestate;
    use temper_runtime::scheduler::DeterministicRng;
    let table = TransitionTable::from_ioa_source(OVERFLOW_CONTRACT);
    for seed in 1..=8 {
        let mut rng = DeterministicRng::new(seed);
        let dir = tempfile::tempdir().unwrap();
        let store = crate::blob_store::BlobStore::local_fs(dir.path());
        let mut state =
            EntityActor::build_initial_state("Document", "seeded", &table, &serde_json::json!({}));
        let mut current = "A".repeat(192 * 1024);
        let initial = process_action_with_blob_prestate(
            &mut state,
            &table,
            "Write",
            &serde_json::json!({"Name":current}),
            &BTreeMap::new(),
            FieldSyncMode::blob_refs_default(),
            crate::blobs::BlobReadSource::Store(&store),
        )
        .await;
        assert!(initial.success);
        crate::blobs::put_overflow_blobs(&store, &initial.overflow_blobs)
            .await
            .unwrap();
        for step in 0..16 {
            let choice = rng.next_bound(8);
            let key = crate::blobs::field_overflow_descriptor(&state.fields["Name"])
                .unwrap()
                .key
                .to_string();
            let original = serde_json::to_vec(&serde_json::json!(current)).unwrap();
            if choice == 5 {
                tokio::fs::remove_file(dir.path().join(&key)).await.unwrap();
            } else if choice == 6 {
                tokio::fs::write(dir.path().join(&key), vec![b'X'; original.len()])
                    .await
                    .unwrap();
            }
            let next = format!("{}{}", rng.next_u64(), "B".repeat(192 * 1024));
            let (action, mut params, accepts) = match choice {
                0 => ("Write", serde_json::json!({"Name":next}), true),
                1 => ("Same", serde_json::json!({"expected":current}), true),
                2 => ("Different", serde_json::json!({"expected":current}), false),
                3 => ("Same", serde_json::json!({"expected":next}), false),
                4 => ("Different", serde_json::json!({"expected":next}), true),
                _ => (
                    if rng.next_bound(2) == 0 {
                        "Same"
                    } else {
                        "Different"
                    },
                    serde_json::json!({"expected":current}),
                    false,
                ),
            };
            if choice == 7 {
                params["undeclared"] = serde_json::json!(rng.next_u64());
            }
            let before = serde_json::to_vec(&state).unwrap();
            let result = process_action_with_blob_prestate(
                &mut state,
                &table,
                action,
                &params,
                &BTreeMap::new(),
                FieldSyncMode::blob_refs_default(),
                crate::blobs::BlobReadSource::Store(&store),
            )
            .await;
            assert_eq!(
                result.success, accepts,
                "seed {seed} step {step} choice {choice}: {:?}",
                result.error
            );
            if accepts {
                crate::blobs::put_overflow_blobs(&store, &result.overflow_blobs)
                    .await
                    .unwrap();
                assert!(result.event.is_some());
                if choice == 0 {
                    current = next;
                }
            } else {
                assert_eq!(
                    serde_json::to_vec(&state).unwrap(),
                    before,
                    "seed {seed} step {step}"
                );
                assert!(result.event.is_none());
                assert!(result.overflow_blobs.is_empty());
                assert!(result.custom_effects.is_empty());
            }
            if matches!(choice, 5 | 6) {
                tokio::fs::write(dir.path().join(key), original)
                    .await
                    .unwrap();
            }
        }
    }
}
