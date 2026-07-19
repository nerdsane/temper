use std::collections::BTreeMap;
use std::time::Duration;

use super::projection_tests::{
    PROJECTED_ITEM_IOA, projected_item_input, replacement_other_input,
    without_projection_declarations,
};
use super::*;
use temper_runtime::persistence::EventStore;
use temper_server::request_context::AgentContext;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityMsg, EntityResponse, StorageStack};
use temper_store_sim::SimEventStore;
#[tokio::test]
async fn startup_vector_backfill_finishes_before_a_new_generation_is_published() {
    let mut state = PlatformState::new(None);
    let store = SimEventStore::no_faults(192);
    state.server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    let tenant = TenantId::new("hot-deploy-projection-test");
    let initial =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(PROJECTED_ITEM_IOA)).await;
    assert!(initial.success, "initial deploy: {}", initial.summary);

    // Hold the per-type fence so the startup worker has already staged the
    // old declarations but cannot publish its result. Its generation read
    // guard must keep the deploy from swapping the registry in that window.
    let projection_fence = store
        .acquire_projection_reconciliation_fence(tenant.as_str(), "Item")
        .await
        .expect("hold projection reconciliation fence");
    let background = state.server.populate_vector_index_from_snapshots(&tenant);
    tokio::pin!(background);
    tokio::select! {
        biased;
        _ = &mut background => panic!("background pass bypassed the held projection fence"),
        _ = tokio::task::yield_now() => {}
    }

    let absent_source = without_projection_declarations();
    let next_input = projected_item_input(&absent_source);
    let deploy = DeployPipeline::verify_and_deploy(&state, &next_input);
    tokio::pin!(deploy);
    tokio::select! {
        biased;
        result = &mut deploy => panic!(
            "generation deploy completed before the staged background pass: {}",
            result.summary
        ),
        _ = tokio::task::yield_now() => {}
    }
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_table(&tenant, "Item")
            .is_some_and(|table| !table.vectors.is_empty()),
        "registry generation overtook a staged old-generation background pass"
    );

    drop(projection_fence);
    background.await;
    let deployed = deploy.await;
    assert!(deployed.success, "removal deploy: {}", deployed.summary);
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_table(&tenant, "Item")
            .is_some_and(|table| table.vectors.is_empty())
    );
}

#[tokio::test]
async fn removed_generation_cannot_write_through_preexisting_actor_ref() {
    let mut state = PlatformState::new(None);
    let store = SimEventStore::no_faults(192);
    state.server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    let tenant = TenantId::new("hot-deploy-projection-test");
    let initial =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(PROJECTED_ITEM_IOA)).await;
    assert!(initial.success, "initial deploy: {}", initial.summary);

    let created = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "detached",
            "Create",
            serde_json::json!({
                "Slug": "before",
                "Embedding": "[1,0,0,0]",
                "EmbeddingModel": "m1"
            }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch create");
    assert!(created.success, "create failed: {:?}", created.error);
    let stale_actor = state
        .server
        .get_or_spawn_tenant_actor(&tenant, "Item", "detached")
        .expect("capture live actor before removal");
    let persistence_id = format!("{tenant}:Item:detached");
    let sequence_before = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read initial journal")
        .len();

    let replacement = DeployPipeline::verify_and_deploy(&state, &replacement_other_input()).await;
    assert!(
        replacement.success,
        "replacement deploy: {}",
        replacement.summary
    );
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_table(&tenant, "Item")
            .is_none(),
        "replacement generation removes Item authority"
    );

    let stale_result: Result<EntityResponse, _> = stale_actor
        .ask(
            EntityMsg::Action {
                name: "Change".to_string(),
                params: serde_json::json!({
                    "Slug": "after",
                    "Embedding": "[0,1,0,0]",
                    "EmbeddingModel": "m1"
                }),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("detached-generation-write".to_string()),
                expected_spec_generation: None,
            },
            Duration::from_secs(1),
        )
        .await;
    assert!(
        stale_result.is_err() || !stale_result.expect("checked successful ask").success,
        "a pre-existing actor ref must not retain write authority after type removal"
    );
    assert_eq!(
        store
            .read_events(&persistence_id, 0)
            .await
            .expect("read journal after stale ask")
            .len(),
        sequence_before,
        "the detached actor must not append after its generation is removed"
    );
    assert!(
        state
            .server
            .get_or_spawn_tenant_actor(&tenant, "Item", "detached")
            .is_none(),
        "the actor cache must not resurrect a removed entity type"
    );
}
