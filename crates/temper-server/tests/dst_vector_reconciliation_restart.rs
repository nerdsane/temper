//! DST: declaration deletion/restart/re-add preserves vector authority.

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EntityVectorRow, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::registry::SpecRegistry;
use temper_server::vector_index::declared_vector_set_signature;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const ITEM_IOA: &str = include_str!("../../../test-fixtures/specs/vectored_item.ioa.toml");
const ITEM_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Dst" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Item">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Embedding" Type="Edm.String"/>
        <Property Name="EmbeddingModel" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="DstService">
        <EntitySet Name="Items" EntityType="Temper.Dst.Item"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

fn registry_with_item(include_item: bool) -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(ITEM_CSDL).expect("parse Item CSDL");
    if include_item {
        registry.register_tenant(
            "default",
            csdl,
            ITEM_CSDL.to_string(),
            &[("Item", ITEM_IOA)],
        );
    } else {
        registry.register_tenant("default", csdl, ITEM_CSDL.to_string(), &[]);
    }
    registry
}

fn state_with_item(store: &SimEventStore, include_item: bool, seed: u64) -> ServerState {
    let mut state = ServerState::from_registry(
        ActorSystem::new(format!("dst-vector-restart-{seed}")),
        registry_with_item(include_item),
    );
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}

#[tokio::test(flavor = "current_thread")]
async fn deleted_vector_declaration_resumes_after_restart_and_readds_identically() {
    // The 100-seed interleaving model lives in `dst_entity_vector_index`; this
    // lifecycle integration adds the real ServerState teardown/reconstruction
    // boundary once, because each state owns long-lived projection queues.
    let seed = 0;
    let (_guard, _clock, _id) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let fingerprint = temper_store_turso::spec_content_hash(ITEM_IOA);
    let table = TransitionTable::from_ioa_source(ITEM_IOA);
    let present_set = declared_vector_set_signature(&table.vectors);
    store.persist_spec_declaration("default", "Item", &fingerprint);

    let state = state_with_item(&store, true, seed);
    let embedding =
        serde_json::to_string(&[1.0f32, 0.0, 0.0, 0.0]).expect("serialize deterministic embedding");
    let persistence_id = "default:Item:item-restart";
    let events = [
        EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: "New".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
        },
        EntityEvent {
            action: "Create".to_string(),
            from_status: "New".to_string(),
            to_status: "Ready".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({
                "Embedding": embedding,
                "EmbeddingModel": "m1",
            }),
            idempotency_key: None,
        },
    ];
    let envelopes = events
        .iter()
        .enumerate()
        .map(|(index, event)| PersistenceEnvelope {
            sequence_nr: (index + 1) as u64,
            event_type: event.action.clone(),
            payload: serde_json::to_value(event).expect("serialize entity event"),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.to_string(),
            },
        })
        .collect::<Vec<_>>();
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &envelopes,
            &[],
            &[EntityVectorRow {
                decl_name: "embed".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
            }],
            true,
            Some(&fingerprint),
        )
        .await
        .expect("seed retained Item journal and vector row");
    state.populate_vector_index_from_snapshots(&tenant).await;
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read initial completion"),
        vec![("Item".to_string(), present_set.clone())]
    );

    store.persist_spec_declaration("default", "Item", "absent:v1");
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            "default",
            parse_csdl(ITEM_CSDL).expect("parse deletion CSDL"),
            ITEM_CSDL.to_string(),
            &[],
        );
    store.fail_next_reads("default:Item:item-restart", 1);
    state.populate_vector_index_from_snapshots(&tenant).await;
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read interrupted deletion completion")
            .is_empty(),
        "seed {seed}: a crashed purge must not publish completion"
    );

    drop(state);
    let restarted = state_with_item(&store, false, seed + 1);
    restarted
        .populate_vector_index_from_snapshots(&tenant)
        .await;
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read resumed deletion completion"),
        vec![("Item".to_string(), "v2|".to_string())],
        "seed {seed}: rebuilt registry revision one must resume the durable tombstone"
    );
    assert!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("read purged candidates")
            .is_empty(),
        "seed {seed}: absent reconciliation must purge retained rows"
    );

    store.persist_spec_declaration("default", "Item", &fingerprint);
    restarted
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            "default",
            parse_csdl(ITEM_CSDL).expect("parse re-add CSDL"),
            ITEM_CSDL.to_string(),
            &[("Item", ITEM_IOA)],
        );
    restarted
        .populate_vector_index_from_snapshots(&tenant)
        .await;
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read re-add completion"),
        vec![("Item".to_string(), present_set)],
        "seed {seed}: identical re-add must claim a newer durable declaration"
    );
    assert_eq!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("read rebuilt candidates")
            .len(),
        1,
        "seed {seed}: re-add must rebuild the retained journal stream"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_journal_event_blocks_vector_completion_watermark() {
    let seed = 217;
    let (_guard, _clock, _id) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let state = state_with_item(&store, true, seed);
    let fingerprint = temper_store_turso::spec_content_hash(ITEM_IOA);
    store.persist_spec_declaration("default", "Item", &fingerprint);
    store
        .append_with_index_rows(
            "default:Item:item-malformed",
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Create".to_string(),
                payload: serde_json::json!({"incompatible": true}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "default:Item:item-malformed".to_string(),
                },
            }],
            &[],
            &[],
            false,
            Some(&fingerprint),
        )
        .await
        .expect("seed malformed durable envelope");

    state
        .populate_vector_index_from_snapshots(&TenantId::default())
        .await;
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read completion claims")
            .is_empty(),
        "strict replay must not watermark a type after skipping an incompatible event"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn truncated_journal_fault_blocks_vector_completion_watermark() {
    let seed = 218;
    let (_guard, _clock, _id) = install_deterministic_context(seed);
    let store = SimEventStore::new(
        seed,
        SimFaultConfig {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 1.0,
            snapshot_failure_prob: 0.0,
        },
    );
    let state = state_with_item(&store, true, seed);
    let fingerprint = temper_store_turso::spec_content_hash(ITEM_IOA);
    store.persist_spec_declaration("default", "Item", &fingerprint);
    let persistence_id = "default:Item:item-truncated";
    let events = [
        EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: "New".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
        },
        EntityEvent {
            action: "Create".to_string(),
            from_status: "New".to_string(),
            to_status: "Ready".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({
                "Embedding": "[1.0,0.0,0.0,0.0]",
                "EmbeddingModel": "m1",
            }),
            idempotency_key: None,
        },
    ];
    let envelopes = events
        .iter()
        .map(|event| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event.action.clone(),
            payload: serde_json::to_value(event).expect("serialize entity event"),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.to_string(),
            },
        })
        .collect::<Vec<_>>();
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &envelopes,
            &[],
            &[],
            false,
            Some(&fingerprint),
        )
        .await
        .expect("seed two-event journal");

    state
        .populate_vector_index_from_snapshots(&TenantId::default())
        .await;
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("read completion claims")
            .is_empty(),
        "a modeled truncated prefix must surface as failure, never as complete replay"
    );
}
