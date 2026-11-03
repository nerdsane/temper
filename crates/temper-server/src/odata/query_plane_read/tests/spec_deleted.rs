//! Only the kernel's tombstone removes a record's catalog row. A record a spec
//! moves into a state named `Deleted` is an ordinary record: its catalog row
//! stays, and so does its id in the entity index. The row used to go while the
//! id stayed, so every list read of the type counted the record as a catalog
//! gap and read every row from source. On production, three removed Members
//! made each `Members?$filter=sub eq '<sub>' and status ne 'Deleted'` load all
//! ~195 Members (about 165 ms) to return one.

use super::*;
use temper_runtime::persistence::EventStore;

/// The Order spec with a `Discard` action into `Deleted`, the way an app spec
/// ends a record (Katagami's Member moves Removing to Deleted on `Removed`).
fn order_spec_with_discard() -> String {
    let spec = ORDER_IOA
        .replacen(
            "\"Refunded\"]\ninitial",
            "\"Refunded\", \"Deleted\"]\ninitial",
            1,
        )
        .replacen(
            "# --- Output Actions",
            "[[action]]\nname = \"Discard\"\nkind = \"input\"\nfrom = [\"Draft\"]\nto = \"Deleted\"\n\n# --- Output Actions",
            1,
        );
    assert!(
        spec.contains("\"Refunded\", \"Deleted\"]") && spec.contains("name = \"Discard\""),
        "the Order fixture changed shape"
    );
    spec
}

fn state_on(store: &TursoEventStore, system_name: &str) -> ServerState {
    let spec = order_spec_with_discard();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TenantId::default().as_str(),
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Order", spec.as_str())],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
}

async fn new_store(name: &str) -> (TursoEventStore, std::path::PathBuf) {
    let db_path = std::env::temp_dir().join(format!("temper-query-plane-{name}-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    (store, db_path)
}

/// Six Orders, then ord-05 discarded into `Deleted`.
async fn six_orders_one_discarded(state: &ServerState) {
    create_orders(state, 6).await;
    state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "ord-05",
            "Discard",
            serde_json::json!({}),
            &AgentContext::for_service("query-plane-spec-deleted"),
        )
        .await
        .expect("discard ord-05");
}

async fn read_orders(state: &ServerState, filter: Option<FilterExpr>) -> QueryPlaneReadResult {
    let query_options = QueryOptions {
        filter,
        top: Some(10),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    match read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant: &TenantId::default(),
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("an Orders list answers, not errors"),
    }
}

/// `Orders('<key>')`'s status, as a read by key answers it.
async fn status_by_key(state: &ServerState, key: &str) -> String {
    match crate::odata::read::load_existing_entity_descriptor_body(
        state,
        &TenantId::default(),
        "Order",
        "Orders",
        key,
    )
    .await
    {
        Ok(body) => body["status"].as_str().unwrap_or_default().to_string(),
        Err(response) => panic!("Orders('{key}') answered {}", response.status()),
    }
}

fn status_eq(value: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(value.to_string()))),
    }
}

fn ids(result: &QueryPlaneReadResult) -> Vec<String> {
    let mut ids: Vec<String> = result.entities.iter().map(listed_entity_id).collect();
    ids.sort();
    ids
}

async fn has_catalog_row(state: &ServerState, entity_id: &str) -> bool {
    let query_plane = state
        .query_plane_store()
        .expect("query-plane store should be configured");
    match load_catalog_rows_by_id(
        &query_plane,
        TenantId::default().as_str(),
        "Order",
        &[entity_id.to_string()],
    )
    .await
    .expect("load catalog row")
    {
        CatalogRowsLoad::Available(rows) => rows.contains_key(entity_id),
        CatalogRowsLoad::Unsupported => panic!("turso catalog row load should be supported"),
    }
}

/// What the reads show of ord-05 and of a filtered read for ord-01: the
/// discarded Order keeps its catalog row, reads by key as `Deleted`, is found
/// by `Status eq 'Deleted'`, and leaves the filtered read with no catalog gap,
/// so the native page answers it loading only the match.
async fn reads(state: &ServerState) -> (bool, String, Vec<String>, (Vec<String>, usize, usize)) {
    let matched = read_orders(state, Some(id_eq_filter("ord-01"))).await;
    (
        has_catalog_row(state, "ord-05").await,
        status_by_key(state, "ord-05").await,
        ids(&read_orders(state, Some(status_eq("Deleted"))).await),
        (
            ids(&matched),
            matched.telemetry.coverage.missing,
            matched.telemetry.materialized_count,
        ),
    )
}

fn expected() -> (bool, String, Vec<String>, (Vec<String>, usize, usize)) {
    (
        true,
        "Deleted".to_string(),
        vec!["ord-05".to_string()],
        (vec!["ord-01".to_string()], 0, 1),
    )
}

/// Discarding an Order keeps its catalog row, so a list of Orders still
/// holds it and a filtered read finds no catalog gap.
#[tokio::test]
async fn a_spec_deleted_record_keeps_its_catalog_row() {
    let (store, db_path) = new_store("spec-deleted-live").await;
    let state = state_on(&store, "query-plane-spec-deleted-live");
    six_orders_one_discarded(&state).await;

    let listed = ids(&read_orders(&state, None).await);
    let shown = reads(&state).await;

    assert_eq!(
        (listed, shown),
        (
            ["ord-00", "ord-01", "ord-02", "ord-03", "ord-04", "ord-05"]
                .map(String::from)
                .to_vec(),
            expected(),
        ),
        "a spec-Deleted Order is an ordinary listed record"
    );

    let _ = std::fs::remove_file(db_path);
}

/// A Temper written before this fix removed ord-05's catalog row when it was
/// discarded. On restart boot repair replays that record from its journal
/// before the server serves and puts the row back, writing nothing to the
/// journal.
#[tokio::test]
async fn a_restart_puts_back_a_spec_deleted_records_catalog_row() {
    let (store, db_path) = new_store("spec-deleted-boot").await;
    six_orders_one_discarded(&state_on(&store, "query-plane-spec-deleted-writer")).await;
    let tenant = TenantId::default();
    // What the kernel used to do on the discard.
    store
        .remove_query_projection(tenant.as_str(), "Order", "ord-05")
        .await
        .expect("remove ord-05's catalog row as the kernel used to");
    let journal = format!("{tenant}:Order:ord-05");
    let events_before = store.read_events(&journal, 0).await.expect("read").len();

    let restarted = state_on(&store, "query-plane-spec-deleted-restarted");
    restarted.populate_index_from_store(&tenant).await;
    assert!(
        restarted
            .list_entity_ids(&tenant, "Order")
            .contains(&"ord-05".to_string())
            && !has_catalog_row(&restarted, "ord-05").await,
        "the setup is a Temper written before the fix: ord-05 listed, with no catalog row"
    );

    restarted.repair_query_projections_at_boot().await;
    let shown = reads(&restarted).await;
    let events_after = store.read_events(&journal, 0).await.expect("read").len();

    assert_eq!(
        (shown, events_after),
        (expected(), events_before),
        "boot repair puts ord-05's catalog row back and leaves its journal as it was"
    );

    let _ = std::fs::remove_file(db_path);
}
