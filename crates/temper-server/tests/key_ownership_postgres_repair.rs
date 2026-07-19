//! Real-Postgres regression for pre-v3 deleted and orphaned key owners.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::key_index::{canonical_key_hash, declared_key_set_signature};
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Arn238" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Doc">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="WorkspaceId" Type="Edm.String"/>
        <Property Name="Path" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Docs" EntityType="Temper.Arn238.Doc"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
const DOC_IOA: &str = include_str!("../../../test-fixtures/specs/keyed_doc.ioa.toml");
const DATA_ONLY_DOC_IOA: &str = r#"
[automaton]
name = "Doc"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "WorkspaceId"
type = "string"
initial = ""

[[state]]
name = "Path"
type = "string"
initial = ""

[[key]]
name = "path"
properties = ["WorkspaceId", "Path"]
"#;

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

fn server_with_postgres_spec(
    tenant: &TenantId,
    store: PostgresEventStore,
    ioa: &'static str,
) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(tenant.as_str(), csdl, CSDL_XML.to_string(), &[("Doc", ioa)]);
    let mut state = ServerState::from_registry(ActorSystem::new("arn238-pg-repair"), registry);
    state.set_storage_stack(StorageStack::from_postgres(store));
    state
}

fn server_with_postgres(tenant: &TenantId, store: PostgresEventStore) -> ServerState {
    server_with_postgres_spec(tenant, store, DOC_IOA)
}

async fn read_migrated_doc_by_key(
    state: &ServerState,
    tenant: &TenantId,
) -> (StatusCode, serde_json::Value) {
    let response = build_router(state.clone())
        .oneshot(
            Request::builder()
                .uri(
                    "/tdata/Docs?$filter=WorkspaceId%20eq%20%27ws%27%20and%20Path%20eq%20%27%2Fmigrated%27",
                )
                .header("x-tenant-id", tenant.as_str())
                .body(Body::empty())
                .expect("catalog-only keyed request"),
        )
        .await
        .expect("catalog-only keyed response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("catalog-only keyed response bytes");
    let body = serde_json::from_slice(&bytes).expect("catalog-only keyed response JSON");
    (status, body)
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
        .expect("append pre-v3 event-only tombstone");
    let stale_snapshot = serde_json::json!({
        "entity_type": "Doc",
        "entity_id": deleted_id,
        "status": "New",
        "item_count": 0,
        "total_event_count": sequence_nr + 2,
        "fields": {"WorkspaceId": "ws", "Path": "/reclaim"},
    });
    store
        .save_snapshot(
            &persistence_id,
            sequence_nr + 2,
            &serde_json::to_vec(&stale_snapshot).expect("serialize stale snapshot"),
        )
        .await
        .expect("seed newer stale live snapshot");
    store
        .upsert_query_projection(
            tenant.as_str(),
            "Doc",
            deleted_id,
            "New",
            &serde_json::json!({"WorkspaceId": "ws", "Path": "/reclaim"}),
            sequence_nr + 3,
        )
        .await
        .expect("seed newest stale live catalog row");

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

/// Migrated deployments can have a durable query-plane catalog row without a
/// journal: ADR-0077 populated the catalog directly from snapshots rather than
/// replaying historical events. Key reconciliation must include that compatibility
/// shape before publishing authoritative coverage, or a keyed miss can hide the
/// existing owner and admit a duplicate claim after restart.
#[tokio::test]
async fn postgres_backfill_reconstructs_catalog_only_owner_before_watermark() {
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
    let store = PostgresEventStore::new(pool);
    let tenant = TenantId::new(format!("arn238-catalog-only-pg-{}", sim_uuid()));
    let state = server_with_postgres(&tenant, store.clone());
    let owner_id = "migrated-owner";
    let hash = key_hash("ws", "/migrated");
    let fields = serde_json::json!({"WorkspaceId": "ws", "Path": "/migrated"});

    store
        .upsert_query_projection(tenant.as_str(), "Doc", owner_id, "New", &fields, 7)
        .await
        .expect("seed migrated catalog-only entity");
    assert!(
        store
            .read_events(&format!("{tenant}:Doc:{owner_id}"), 0)
            .await
            .expect("catalog-only journal read")
            .is_empty(),
        "precondition: migrated owner has no journal"
    );

    let (pre_repair_status, pre_repair_body) = read_migrated_doc_by_key(&state, &tenant).await;
    assert_eq!(pre_repair_status, StatusCode::OK);
    assert_eq!(
        pre_repair_body["value"][0]["entity_id"], owner_id,
        "scan-safe reads must materialize catalog-only migration owners before coverage"
    );

    state.populate_key_index_from_snapshots(&tenant).await;

    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Doc", "path", &hash)
            .await
            .expect("reconstructed key lookup"),
        Some(owner_id.to_string()),
        "repair must reconstruct the durable catalog-only owner before coverage"
    );
    let table = TransitionTableForTest::doc();
    let signature = declared_key_set_signature(&table.keys);
    assert!(
        store
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("watermarks")
            .contains(&("Doc".to_string(), signature)),
        "coverage may publish only after the catalog-only owner is indexed"
    );

    let (post_repair_status, post_repair_body) = read_migrated_doc_by_key(&state, &tenant).await;
    assert_eq!(post_repair_status, StatusCode::OK);
    assert_eq!(
        post_repair_body["value"][0]["entity_id"], owner_id,
        "the fenced key hit must materialize the exact catalog generation"
    );

    let duplicate = state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "duplicate",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/migrated"}),
        )
        .await;
    assert!(
        duplicate.is_err(),
        "the reconstructed durable owner must reject a duplicate claim"
    );
}

/// A legacy field-index row proves that an entity exists but does not contain a
/// complete state object from which every declared key component can be derived.
/// Repair must keep reads scan-safe by withholding coverage instead of certifying
/// that the partial row owns no key.
#[tokio::test]
async fn postgres_backfill_withholds_watermark_for_field_index_only_entity() {
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
    let tenant = TenantId::new(format!("arn238-field-only-pg-{}", sim_uuid()));
    let state = server_with_postgres(&tenant, store.clone());

    sqlx::query(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) \
         VALUES ($1, 'Doc', 'partial-owner', 'WorkspaceId', 'ws', 'New')",
    )
    .bind(tenant.as_str())
    .execute(&pool)
    .await
    .expect("seed field-index-only compatibility row");

    state.populate_key_index_from_snapshots(&tenant).await;

    let table = TransitionTableForTest::doc();
    let signature = declared_key_set_signature(&table.keys);
    assert!(
        !store
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("watermarks")
            .contains(&("Doc".to_string(), signature)),
        "a field-index-only entity cannot be reconstructed and must block coverage"
    );
}

/// The native PostgreSQL data-only optimization must claim a declared key in the
/// same transaction as its first event and projection. A conflicting claim rolls
/// all three writes back.
#[tokio::test]
async fn postgres_native_data_only_create_co_commits_declared_keys() {
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
    let store = PostgresEventStore::new(pool);
    let tenant = TenantId::new(format!("arn238-data-only-pg-{}", sim_uuid()));
    let state = server_with_postgres_spec(&tenant, store.clone(), DATA_ONLY_DOC_IOA);
    let hash = key_hash("ws", "/native");

    let created = state
        .try_create_data_only_tenant_entity(
            &tenant,
            "Doc",
            "native-a",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/native"}),
        )
        .await
        .expect("native data-only create result")
        .expect("eligible native data-only create");
    assert!(created.success);
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Doc", "path", &hash)
            .await
            .expect("native key lookup"),
        Some("native-a".to_string())
    );

    let duplicate = state
        .try_create_data_only_tenant_entity(
            &tenant,
            "Doc",
            "native-b",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/native"}),
        )
        .await;
    assert!(duplicate.is_err(), "a duplicate native claim must fail");
    assert!(
        store
            .read_events(&format!("{tenant}:Doc:native-b"), 0)
            .await
            .expect("duplicate journal read")
            .is_empty(),
        "the conflicting native transaction must not insert an event"
    );
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Doc", "path", &hash)
            .await
            .expect("owner after duplicate"),
        Some("native-a".to_string()),
        "the rejected transaction must preserve the original owner"
    );
}

struct TransitionTableForTest;

impl TransitionTableForTest {
    fn doc() -> temper_jit::table::TransitionTable {
        temper_jit::table::TransitionTable::from_ioa_source(DOC_IOA)
    }
}
