//! Real-Postgres regression for pre-v2 deleted and orphaned key owners.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::key_index::{canonical_key_hash, declared_key_set_signature};
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const DOC_IOA: &str = include_str!("../../../test-fixtures/specs/keyed_doc.ioa.toml");

fn key_hash(workspace: &str, path: &str) -> String {
    canonical_key_hash(
        "path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        serde_json::json!({"WorkspaceId": workspace, "Path": path})
            .as_object()
            .expect("key fields"),
    )
    .expect("complete key")
}

fn server_with_postgres(tenant: &TenantId, store: PostgresEventStore) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("arn238-pg-repair"), registry);
    state.set_storage_stack(StorageStack::from_postgres(store));
    state
}

/// A type cannot receive a complete watermark while stale rows owned by a deleted
/// journal stream or a key-index-only phantom remain outside the repair enumeration.
#[tokio::test]
async fn postgres_backfill_purges_deleted_and_orphaned_key_owners_before_watermark() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect to Postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("run Postgres migrations");
    let store = PostgresEventStore::new(pool.clone());
    let tenant = TenantId::new(format!("arn238-key-repair-{}", sim_uuid()));
    let state = server_with_postgres(&tenant, store.clone());
    let deleted_id = "legacy-deleted";
    let deleted_hash = key_hash("ws", "/reclaim");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            deleted_id,
            serde_json::json!({"WorkspaceId": "ws", "Path": "/reclaim"}),
        )
        .await
        .expect("create legacy key owner");
    let persistence_id = format!("{tenant}:Doc:{deleted_id}");
    let sequence_nr = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read legacy history")
        .last()
        .expect("created event")
        .sequence_nr;
    let timestamp = sim_now();
    let deleted = EntityEvent {
        action: "Deleted".to_string(),
        from_status: "New".to_string(),
        to_status: "Deleted".to_string(),
        timestamp,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            sequence_nr,
            &[PersistenceEnvelope {
                sequence_nr: sequence_nr + 1,
                event_type: "Deleted".to_string(),
                payload: serde_json::to_value(deleted).expect("serialize tombstone"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("append pre-v2 event-only tombstone");

    let orphan_hash = key_hash("ws", "/orphan");
    sqlx::query(
        "INSERT INTO entity_key_index \
         (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
         VALUES ($1, 'Doc', 'path', $2, 'orphan-owner', 0)",
    )
    .bind(tenant.as_str())
    .bind(&orphan_hash)
    .execute(&pool)
    .await
    .expect("seed key-index-only orphan");

    for (hash, owner) in [(&deleted_hash, deleted_id), (&orphan_hash, "orphan-owner")] {
        assert_eq!(
            store
                .lookup_by_key(tenant.as_str(), "Doc", "path", hash)
                .await
                .expect("legacy key lookup"),
            Some(owner.to_string()),
            "precondition: stale owner is present before repair"
        );
    }

    state.populate_key_index_from_snapshots(&tenant).await;

    for hash in [&deleted_hash, &orphan_hash] {
        assert_eq!(
            store
                .lookup_by_key(tenant.as_str(), "Doc", "path", hash)
                .await
                .expect("repaired key lookup"),
            None,
            "repair must purge deleted and key-index-only owners"
        );
    }
    let table = TransitionTableForTest::doc();
    let signature = declared_key_set_signature(&table.keys);
    assert!(
        store
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("watermarks")
            .contains(&("Doc".to_string(), signature)),
        "the watermark is valid only after every stale owner is purged"
    );

    let reclaimed = state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "replacement",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/reclaim"}),
        )
        .await
        .expect("reclaim formerly stale key");
    assert!(reclaimed.success);
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Doc", "path", &deleted_hash)
            .await
            .expect("reclaimed lookup"),
        Some("replacement".to_string())
    );
}

struct TransitionTableForTest;

impl TransitionTableForTest {
    fn doc() -> temper_jit::table::TransitionTable {
        temper_jit::table::TransitionTable::from_ioa_source(DOC_IOA)
    }
}
