//! Upgrade regression for exact key/vector projection reconciliation.
//!
//! Older binaries could watermark a projection while a deleted journal or an
//! indexed-only phantom still retained derived rows. A restart on the fixed binary
//! must invalidate that legacy watermark, enumerate both authoritative journals and
//! projection rows, and purge everything that replay says is absent.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EntityKeyRow, EntityVectorRow, EventStore};
use temper_runtime::tenant::TenantId;
use temper_server::key_index::canonical_key_hash;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["New", "Ready", "Deleted"]
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
name = "Delete"
kind = "input"
from = ["Ready"]
to = "Deleted"
"#;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
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
</edmx:Edmx>
"#;

fn build_state() -> (ServerState, SimEventStore) {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Item", ITEM_IOA)],
    );
    let store = SimEventStore::no_faults(189);
    let mut state = ServerState::from_registry(ActorSystem::new("legacy-projection"), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

async fn seed_deleted_item(state: &ServerState) {
    let tenant = TenantId::default();
    let context = AgentContext::for_service("legacy-projection-test");
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "dead",
            "Create",
            serde_json::json!({
                "Slug": "dead-slug",
                "Embedding": "[1,0,0,0]",
                "EmbeddingModel": "m1"
            }),
            &context,
        )
        .await
        .expect("create dispatch");
    assert!(create.success, "create failed: {:?}", create.error);
    let delete = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "dead",
            "Delete",
            serde_json::json!({}),
            &context,
        )
        .await
        .expect("delete dispatch");
    assert!(delete.success, "delete failed: {:?}", delete.error);
}

fn slug_hash(slug: &str) -> String {
    let fields = serde_json::Map::from_iter([("Slug".to_string(), serde_json::json!(slug))]);
    canonical_key_hash("slug", &["Slug".to_string()], &fields).expect("complete slug key")
}

async fn seed_stale_keys(store: &SimEventStore) {
    for (entity_id, slug) in [("dead", "dead-slug"), ("phantom", "phantom-slug")] {
        store
            .backfill_entity_keys(
                "default",
                "Item",
                entity_id,
                &[EntityKeyRow {
                    key_name: "slug".to_string(),
                    key_hash: slug_hash(slug),
                }],
            )
            .await
            .expect("seed historical key row");
    }
    // Exact signature written by pre-fix binaries.
    store
        .mark_key_index_backfilled("default", "Item", "slug")
        .await
        .expect("seed legacy key watermark");
}

async fn seed_stale_vectors(store: &SimEventStore) {
    for entity_id in ["dead", "phantom"] {
        store
            .backfill_entity_vectors(
                "default",
                "Item",
                entity_id,
                &[EntityVectorRow {
                    decl_name: "embed".to_string(),
                    model_tag: "m1".to_string(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                }],
            )
            .await
            .expect("seed historical vector row");
    }
    // Exact signature written by pre-fix binaries.
    store
        .mark_vector_index_backfilled("default", "Item", "embed:Embedding:EmbeddingModel:4:cosine")
        .await
        .expect("seed legacy vector watermark");
}

#[tokio::test]
async fn legacy_key_watermark_reconciles_tombstone_and_index_only_rows() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    seed_deleted_item(&state).await;
    seed_stale_keys(&store).await;

    state.populate_key_index_from_snapshots(&tenant).await;

    for slug in ["dead-slug", "phantom-slug"] {
        assert_eq!(
            store
                .lookup_by_key("default", "Item", "slug", &slug_hash(slug))
                .await
                .expect("key lookup"),
            None,
            "replay-absent entity must not retain key {slug} after upgrade"
        );
    }
}

#[tokio::test]
async fn legacy_vector_watermark_reconciles_tombstone_and_index_only_rows() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    seed_deleted_item(&state).await;
    seed_stale_vectors(&store).await;

    state.populate_vector_index_from_snapshots(&tenant).await;

    assert!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("vector candidates")
            .is_empty(),
        "replay-absent tombstone and phantom vectors must be purged after upgrade"
    );
}
