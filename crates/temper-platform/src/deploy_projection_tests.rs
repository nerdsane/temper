use std::collections::BTreeMap;

use super::tests::{TASK_CSDL, TASK_IOA};
use super::*;
use temper_runtime::persistence::EventStore;
use temper_server::StorageStack;
use temper_server::request_context::AgentContext;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_store_sim::SimEventStore;

pub(super) const PROJECTED_ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "Slug"
type = "string"
initial = ""

[[state]]
name = "Embedding"
type = "string"
initial = ""

[[state]]
name = "EmbeddingModel"
type = "string"
initial = ""

[[key]]
name = "slug"
properties = ["Slug"]

[[vector]]
name = "embed"
property = "Embedding"
model_property = "EmbeddingModel"
dims = 4
metric = "cosine"

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Slug", "Embedding", "EmbeddingModel"]

[[action]]
name = "Change"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Slug", "Embedding", "EmbeddingModel"]
"#;

const PROJECTED_ITEM_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Item">
    <Key><PropertyRef Name="Id"/></Key>
    <Property Name="Id" Type="Edm.String" Nullable="false"/>
    <Property Name="Slug" Type="Edm.String"/>
    <Property Name="Embedding" Type="Edm.String"/>
    <Property Name="EmbeddingModel" Type="Edm.String"/>
    <Property Name="Status" Type="Edm.String"/>
  </EntityType>
  <EntityContainer Name="TestService">
    <EntitySet Name="Items" EntityType="Temper.Test.Item"/>
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

pub(super) fn projected_item_input(ioa_source: &str) -> DeployInput {
    DeployInput {
        tenant_name: "hot-deploy-projection-test".into(),
        csdl_xml: PROJECTED_ITEM_CSDL.into(),
        entities: vec![EntitySpecSource {
            entity_type: "Item".into(),
            ioa_source: ioa_source.into(),
        }],
        wasm_modules: BTreeMap::new(),
    }
}

pub(super) fn replacement_other_input() -> DeployInput {
    DeployInput {
        tenant_name: "hot-deploy-projection-test".into(),
        csdl_xml: TASK_CSDL.replace("Task", "Other"),
        entities: vec![EntitySpecSource {
            entity_type: "Other".into(),
            ioa_source: TASK_IOA.replace("Task", "Other"),
        }],
        wasm_modules: BTreeMap::new(),
    }
}

pub(super) fn without_projection_declarations() -> String {
    PROJECTED_ITEM_IOA
        .replace(
            "[[key]]\nname = \"slug\"\nproperties = [\"Slug\"]\n\n",
            "",
        )
        .replace(
            "[[vector]]\nname = \"embed\"\nproperty = \"Embedding\"\nmodel_property = \"EmbeddingModel\"\ndims = 4\nmetric = \"cosine\"\n\n",
            "",
        )
}

#[tokio::test]
async fn hot_deploy_serializes_removed_projection_retirement_before_identical_readd() {
    let mut state = PlatformState::new(None);
    let store = SimEventStore::no_faults(189);
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
            "item-1",
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

    let absent_source = without_projection_declarations();
    let removed =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(&absent_source)).await;
    assert!(removed.success, "removal deploy: {}", removed.summary);
    assert_eq!(
        store
            .key_index_backfilled_types(tenant.as_str())
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|[]".to_string())]
    );
    assert_eq!(
        store
            .vector_index_backfilled_types(tenant.as_str())
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|[]".to_string())]
    );

    let changed = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "item-1",
            "Change",
            serde_json::json!({
                "Slug": "after",
                "Embedding": "[0,1,0,0]",
                "EmbeddingModel": "m1"
            }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch change while declarations absent");
    assert!(changed.success, "change failed: {:?}", changed.error);

    let readded =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(PROJECTED_ITEM_IOA)).await;
    assert!(readded.success, "re-add deploy: {}", readded.summary);

    let slug_hash = |slug: &str| {
        let fields = serde_json::json!({"Slug": slug});
        temper_server::key_index::canonical_key_hash(
            "slug",
            &["Slug".to_string()],
            fields.as_object().unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Item", "slug", &slug_hash("before"),)
            .await
            .unwrap(),
        None,
        "stale key from declaration-absent writes must be removed"
    );
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Item", "slug", &slug_hash("after"))
            .await
            .unwrap()
            .as_deref(),
        Some("item-1")
    );
    let vectors = store
        .vector_candidates(tenant.as_str(), "Item", "embed", "m1", 10)
        .await
        .unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].entity_id, "item-1");
    assert_eq!(vectors[0].vector, vec![0.0, 1.0, 0.0, 0.0]);
}

#[tokio::test]
async fn failed_exact_reconciliation_does_not_block_corrective_generation() {
    let mut state = PlatformState::new(None);
    let store = SimEventStore::no_faults(190);
    state.server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
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
    let absent_source = without_projection_declarations();

    let initial =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(&absent_source)).await;
    assert!(initial.success, "initial deploy: {}", initial.summary);
    for entity_id in ["duplicate-1", "duplicate-2"] {
        let created = state
            .server
            .dispatch_tenant_action(
                &tenant,
                "Item",
                entity_id,
                "Create",
                serde_json::json!({
                    "Slug": "duplicate",
                    "Embedding": "[1,0,0,0]",
                    "EmbeddingModel": "m1"
                }),
                &AgentContext::default(),
            )
            .await
            .expect("dispatch create");
        assert!(created.success, "create failed: {:?}", created.error);
    }

    let unreconcilable =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(PROJECTED_ITEM_IOA)).await;
    assert!(
        !unreconcilable.success,
        "duplicate live keys must fail exact reconciliation"
    );
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_table(&tenant, "Item")
            .is_some_and(|table| !table.keys.is_empty()),
        "the failed generation is already live and requires remediation"
    );

    let corrective =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(&absent_source)).await;
    assert!(
        corrective.success,
        "a failed exact rebuild must not deadlock corrective removal: {}",
        corrective.summary
    );
}

#[tokio::test]
async fn in_flight_old_generation_append_precedes_hot_swap_reconciliation() {
    let mut state = PlatformState::new(None);
    let store = SimEventStore::no_faults(191);
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
    let absent_source = without_projection_declarations();
    let initial =
        DeployPipeline::verify_and_deploy(&state, &projected_item_input(&absent_source)).await;
    assert!(initial.success, "initial deploy: {}", initial.summary);
    let created = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "in-flight",
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

    let gate = store.inject_append_gate(&format!("{tenant}:Item:in-flight"));
    let change_agent = AgentContext::default();
    let change = state.server.dispatch_tenant_action(
        &tenant,
        "Item",
        "in-flight",
        "Change",
        serde_json::json!({
            "Slug": "after",
            "Embedding": "[0,1,0,0]",
            "EmbeddingModel": "m1"
        }),
        &change_agent,
    );
    let projected_input = projected_item_input(PROJECTED_ITEM_IOA);
    let deploy_after_append_enters = async {
        gate.wait_until_blocked().await;
        let deploy = DeployPipeline::verify_and_deploy(&state, &projected_input);
        let release = async {
            // The deploy future is polled first by `join!`. With the writer
            // barrier it reaches the generation write lock and waits here;
            // without the barrier it swaps and reconciles before this runs.
            tokio::task::yield_now().await;
            assert!(
                state
                    .registry
                    .read()
                    .unwrap()
                    .get_table(&tenant, "Item")
                    .is_some_and(|table| table.keys.is_empty()),
                "registry generation overtook an in-flight old-generation append"
            );
            gate.release().await;
        };
        let (result, ()) = tokio::join!(deploy, release);
        result
    };
    let (changed, deployed) = tokio::join!(change, deploy_after_append_enters);
    let changed = changed.expect("dispatch change");
    assert!(changed.success, "change failed: {:?}", changed.error);
    assert!(deployed.success, "projected deploy: {}", deployed.summary);

    let fields = serde_json::json!({"Slug": "after"});
    let hash = temper_server::key_index::canonical_key_hash(
        "slug",
        &["Slug".to_string()],
        fields.as_object().unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Item", "slug", &hash)
            .await
            .unwrap()
            .as_deref(),
        Some("in-flight"),
        "the new watermark must include the append that entered under the old generation"
    );
}
