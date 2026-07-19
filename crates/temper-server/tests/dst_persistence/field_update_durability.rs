use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_sim::SimFaultConfig;

use super::field_update_support::*;
use super::*;

// =========================================================================
// Regression: PATCH-only fields survive actor replacement
// =========================================================================

#[tokio::test]
async fn dst_patch_only_fields_survive_actor_replacement() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store = sim_store(seed);
        let table = order_table();
        let entity_id = format!("ord-patch-only-{seed}");

        {
            let system = ActorSystem::new("dst-patch-only-1");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({
                    "Title": "before-patch",
                    "StableField": "preserved",
                    "WorkspaceId": "ws-patch",
                    "Path": "/before"
                }),
                store.clone(),
                BackendLabel::Sim,
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            let patched = update_fields(
                &actor_ref,
                serde_json::json!({
                    "Title": "after-patch",
                    "Priority": "High",
                    "Path": "/after"
                }),
                false,
            )
            .await;
            assert!(
                patched.success,
                "seed {seed}: PATCH failed: {:?}",
                patched.error
            );
            assert_eq!(patched.state.fields["Title"], "after-patch");
            assert_eq!(patched.state.fields["Priority"], "High");
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/before"),
                    )
                    .await
                    .expect("old key lookup should succeed"),
                None,
                "seed {seed}: old declared-key projection was not removed"
            );
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/after"),
                    )
                    .await
                    .expect("new key lookup should succeed"),
                Some(entity_id.clone()),
                "seed {seed}: PATCH did not co-commit its declared-key projection"
            );
        }

        {
            let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 10_000);
            let system = ActorSystem::new("dst-patch-only-2");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
                BackendLabel::Sim,
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, format!("{entity_id}-replacement"));

            let replayed = get_state(&actor_ref).await;
            assert_eq!(
                replayed.state.fields["Title"], "after-patch",
                "seed {seed}: acknowledged PATCH was lost after actor replacement"
            );
            assert_eq!(
                replayed.state.fields["Priority"], "High",
                "seed {seed}: PATCH-only field was lost after actor replacement"
            );
            assert_eq!(replayed.state.fields["StableField"], "preserved");

            let replaced =
                update_fields(&actor_ref, serde_json::json!({"Title": "after-put"}), true).await;
            assert!(replaced.success, "seed {seed}: PUT failed");
            assert!(replaced.state.fields.get("Priority").is_none());
            assert!(replaced.state.fields.get("StableField").is_none());
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/after"),
                    )
                    .await
                    .expect("removed key lookup should succeed"),
                None,
                "seed {seed}: PUT left a stale declared-key projection"
            );
        }

        let (_guard3, _clock3, _id_gen3) = install_deterministic_context(seed + 20_000);
        let system = ActorSystem::new("dst-patch-only-3");
        let actor = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, format!("{entity_id}-put-replacement"));
        let replayed = get_state(&actor_ref).await;
        assert_eq!(replayed.state.fields["Title"], "after-put");
        assert!(
            replayed.state.fields.get("Priority").is_none(),
            "seed {seed}: PUT replay resurrected a removed PATCH field"
        );
        assert!(replayed.state.fields.get("StableField").is_none());
        assert_eq!(
            store
                .lookup_by_key(
                    "default",
                    "Order",
                    "ws_path",
                    &order_key_hash("ws-patch", "/after"),
                )
                .await
                .expect("replayed removed key lookup should succeed"),
            None,
            "seed {seed}: stale key returned after PUT replay"
        );
    }
}

#[tokio::test]
async fn dst_delete_co_commits_declared_key_projection_purge() {
    let seed = 18_907;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store_inner = SimEventStore::no_faults(seed);
    let store = BoxedEventStore::new(store_inner.clone());
    let entity_id = "ord-delete-key-projection";
    let persistence_id = format!("default:Order:{entity_id}");
    let key_hash = order_key_hash("ws-delete", "/durable-key");
    let system = ActorSystem::new("dst-delete-key-projection");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({
            "WorkspaceId": "ws-delete",
            "Path": "/durable-key"
        }),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    assert_eq!(
        store_inner
            .lookup_by_key("default", "Order", "ws_path", &key_hash)
            .await
            .expect("lookup live key"),
        Some(entity_id.to_string())
    );

    let deleted = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("delete keyed entity");
    assert!(deleted.success, "delete failed: {:?}", deleted.error);
    assert_eq!(deleted.state.status, "Deleted");
    assert_eq!(deleted.state.sequence_nr, 2);
    assert_eq!(
        store_inner
            .lookup_by_key("default", "Order", "ws_path", &key_hash)
            .await
            .expect("lookup purged key"),
        None,
        "the tombstone append must purge the entity's declared-key row"
    );
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_delete_co_commits_vector_projection_purge() {
    let seed = 18_908;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store_inner = SimEventStore::no_faults(seed);
    let store = BoxedEventStore::new(store_inner.clone());
    let entity_id = "item-delete-vector-projection";
    let persistence_id = format!("default:Item:{entity_id}");
    let system = ActorSystem::new("dst-delete-vector-projection");
    let actor = EntityActor::with_persistence(
        "Item",
        entity_id,
        vectored_item_table(),
        serde_json::json!({
            "Embedding": [1.0, 0.0, 0.0, 0.0],
            "EmbeddingModel": "model-delete"
        }),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    let candidates = store_inner
        .vector_candidates("default", "Item", "embed", "model-delete", 10)
        .await
        .expect("read live vector candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].entity_id, entity_id);

    let deleted = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("delete vectored entity");
    assert!(deleted.success, "delete failed: {:?}", deleted.error);
    assert_eq!(deleted.state.status, "Deleted");
    assert_eq!(deleted.state.sequence_nr, 2);
    assert!(
        store_inner
            .vector_candidates("default", "Item", "embed", "model-delete", 10)
            .await
            .expect("read purged vector candidates")
            .is_empty(),
        "the tombstone append must purge the entity's vector row"
    );
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_tombstone_rejects_field_updates_and_repeated_delete_is_idempotent() {
    let seed = 18_909;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let store_inner = SimEventStore::no_faults(seed);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let entity_id = "ord-terminal-tombstone";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-terminal-tombstone");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table.clone(),
        serde_json::json!({"Title": "durable-before"}),
        store.clone(),
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    assert_eq!(get_state(&actor_ref).await.state.sequence_nr, 1);
    let deleted = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("delete entity");
    assert!(deleted.success);
    assert_eq!(deleted.state.sequence_nr, 2);

    let repeated = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("repeat delete");
    assert!(repeated.success, "repeated delete should be idempotent");
    assert_eq!(repeated.state.sequence_nr, 2);

    let update = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "must-not-append"}),
        false,
    )
    .await;
    assert!(!update.success);
    assert_eq!(
        update.error.as_deref(),
        Some("deleted entity cannot be updated")
    );
    assert_eq!(update.state.sequence_nr, 2);
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);

    let replacement = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let replacement_ref = system.spawn(replacement, "replacement-after-terminal-tombstone");
    let recovered = get_state(&replacement_ref).await;
    assert_eq!(recovered.state.status, "Deleted");
    assert_eq!(recovered.state.sequence_nr, 2);
    assert_eq!(recovered.state.fields["Title"], "durable-before");
}

#[tokio::test]
async fn dst_delete_storage_error_after_commit_recovers_the_tombstone() {
    let seed = 18_910;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-ambiguous-delete";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-ambiguous-delete");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    assert_eq!(get_state(&actor_ref).await.state.sequence_nr, 1);
    scripted_store.commit_then_report_storage_failure(&persistence_id);
    let deleted = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("ambiguous delete should recover authoritative history");
    assert!(deleted.success, "delete failed: {:?}", deleted.error);
    assert_eq!(deleted.state.status, "Deleted");
    assert_eq!(deleted.state.sequence_nr, 2);
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);

    let live = get_state(&actor_ref).await;
    assert_eq!(live.state.status, "Deleted");
    assert_eq!(live.state.sequence_nr, 2);
    let repeated = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::Delete {
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("repeated delete after recovery");
    assert!(repeated.success);
    assert_eq!(repeated.state.sequence_nr, 2);
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

// =========================================================================
// Regression: a failed journal append cannot publish a field update
// =========================================================================

#[tokio::test]
async fn dst_field_update_fails_closed_when_journal_append_fails() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(189);
    let store_inner = SimEventStore::no_faults(189);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let system = ActorSystem::new("dst-field-update-fail-closed");
    let actor = EntityActor::with_persistence(
        "Order",
        "ord-field-failure",
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "ord-field-failure");

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.fields["Title"], "durable-before");
    let sequence_before = before.state.sequence_nr;

    store_inner.restore_faults(SimFaultConfig {
        write_failure_prob: 1.0,
        ..SimFaultConfig::none()
    });

    let response = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "volatile-after"}),
        false,
    )
    .await;

    assert!(
        !response.success,
        "field update must not report success when its journal append fails"
    );
    assert_eq!(response.state.fields["Title"], "durable-before");
    assert_eq!(response.state.sequence_nr, sequence_before);

    let live = get_state(&actor_ref).await;
    assert_eq!(live.state.fields["Title"], "durable-before");
    assert_eq!(live.state.sequence_nr, sequence_before);
}

#[tokio::test]
async fn dst_field_update_retries_after_concurrency_violation() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_900);
    let store_inner = SimEventStore::no_faults(18_900);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let entity_id = "ord-field-concurrency";
    let persistence_id = format!("default:Order:{entity_id}");

    {
        let system = ActorSystem::new("dst-field-update-concurrency-1");
        let actor = EntityActor::with_persistence(
            "Order",
            entity_id,
            table.clone(),
            serde_json::json!({"Title": "before-retry"}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, entity_id);
        let before = get_state(&actor_ref).await;
        assert_eq!(before.state.sequence_nr, 1);

        store_inner
            .append(
                &persistence_id,
                1,
                &[PersistenceEnvelope {
                    sequence_nr: 2,
                    event_type: "AddItem".to_string(),
                    payload: serde_json::json!({
                        "action": "AddItem",
                        "from_status": "Draft",
                        "to_status": "Draft",
                        "timestamp": sim_now(),
                        "params": {"ProductId": "concurrent-item"}
                    }),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                }],
            )
            .await
            .expect("concurrent action should advance authoritative history");
        let response = update_fields(
            &actor_ref,
            serde_json::json!({"Title": "after-retry"}),
            false,
        )
        .await;
        assert!(
            response.success,
            "field update retry failed: {:?}",
            response.error
        );
        assert_eq!(response.state.item_count, 1);
        assert_eq!(response.state.fields["ProductId"], "concurrent-item");
    }

    let system = ActorSystem::new("dst-field-update-concurrency-2");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "ord-field-concurrency-replacement");
    let replayed = get_state(&actor_ref).await;
    assert_eq!(replayed.state.fields["Title"], "after-retry");
    assert_eq!(replayed.state.item_count, 1);
    assert_eq!(replayed.state.fields["ProductId"], "concurrent-item");
}
