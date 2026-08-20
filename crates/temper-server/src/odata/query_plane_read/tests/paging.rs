//! Integration tests for server-driven paging (`@odata.nextLink` / keyset
//! `$skiptoken`, ARN-160) and the projected-vs-canonical read invariant
//! (ARN-97), against a real Turso store.
//!
//! The load-bearing property: following the continuation from the first page to
//! the last enumerates exactly the same entity set as one big `$top` read — no
//! duplicates, no gaps — for an unfiltered read, a filtered + `$orderby` read,
//! and the same read under `$select`.

use super::*;
use crate::request_context::AgentContext;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;

fn body_entity_id(entity: &serde_json::Value) -> String {
    if let Some(id) = entity.get("entity_id").and_then(|v| v.as_str()) {
        return id.to_string();
    }
    if let Some(id) = entity
        .get("fields")
        .and_then(|f| f.get("Id"))
        .and_then(|v| v.as_str())
    {
        return id.to_string();
    }
    // Fall back to the @odata.id annotation, which `$select` always preserves:
    // `Orders('ord-00')`.
    let odata_id = entity["@odata.id"].as_str().unwrap_or_default();
    odata_id
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .map(|inner| inner.trim_matches('\'').to_string())
        .unwrap_or_default()
}

/// Follow the continuation from the first page to the last, returning the
/// concatenated entity ids in the order the pages delivered them.
async fn walk_pages(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    base: &QueryOptions,
    budget: QueryPlaneReadBudget,
) -> Vec<String> {
    let page_size = budget.requested_top(base);
    let mut ids = Vec::new();
    let mut skiptoken: Option<String> = None;
    loop {
        let options = QueryOptions {
            skiptoken: skiptoken.clone(),
            ..base.clone()
        };
        let result = read_entity_set_from_query_plane(QueryPlaneReadRequest {
            state,
            tenant,
            security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &options,
            budget,
        })
        .await
        .unwrap_or_else(|_| panic!("page read should succeed"));

        assert!(
            result.entities.len() <= page_size,
            "a page must never exceed the requested page size"
        );
        for entity in &result.entities {
            ids.push(body_entity_id(entity));
        }
        match result.next_skiptoken {
            Some(token) => {
                assert_eq!(
                    result.entities.len(),
                    page_size,
                    "only a full page may carry a continuation"
                );
                skiptoken = Some(token);
            }
            None => break,
        }
        assert!(ids.len() <= 1000, "pagination failed to terminate");
    }
    ids
}

async fn single_read(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    base: &QueryOptions,
    budget: QueryPlaneReadBudget,
) -> Vec<String> {
    let options = QueryOptions {
        top: Some(1000),
        ..base.clone()
    };
    let result = read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget,
    })
    .await
    .unwrap_or_else(|_| panic!("single big read should succeed"));
    // The one big read must not itself advertise a continuation.
    assert!(result.next_skiptoken.is_none(), "big read was truncated");
    result.entities.iter().map(body_entity_id).collect()
}

async fn turso_state(system_name: &str) -> (ServerState, TursoEventStore, String) {
    let db_path = std::env::temp_dir().join(format!("temper-paging-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store, db_path.display().to_string())
}

/// Commit an Order to the journal (so `list_entity_ids` enumerates it) and
/// project its test fields into the catalog.
async fn seed_order(
    state: &ServerState,
    store: &TursoEventStore,
    tenant: &TenantId,
    id: &str,
    fields: serde_json::Value,
) {
    let agent_ctx = AgentContext::for_service("paging-test");
    state
        .dispatch_tenant_action(
            tenant,
            "Order",
            id,
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("create order");
    upsert_order_projection(store, tenant, id, fields, 1).await;
}

/// Unfiltered list: the default entity_id-ascending order paginates cleanly and
/// the union of pages equals one big read. This is the ARN-160 shape — a page of
/// 100 with no nextLink made agents read a 214-item catalog as 100.
#[tokio::test]
async fn unfiltered_pagination_enumerates_the_whole_set() {
    let (state, store, db_path) = turso_state("paging-unfiltered").await;
    let tenant = TenantId::default();
    for index in 0..7usize {
        seed_order(
            &state,
            &store,
            &tenant,
            &format!("ord-{index:02}"),
            serde_json::json!({}),
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 2,
        max_entities: 100,
    };
    let base = QueryOptions::default();

    let paged = walk_pages(&state, &tenant, &security_ctx, &base, budget).await;
    let whole = single_read(&state, &tenant, &security_ctx, &base, budget).await;

    assert_eq!(whole.len(), 7);
    assert_eq!(paged, whole, "paged union must equal the single big read");
    let unique: std::collections::BTreeSet<_> = paged.iter().collect();
    assert_eq!(unique.len(), paged.len(), "no duplicates across pages");

    let _ = std::fs::remove_file(db_path);
}

/// Filtered + `$orderby` list (the production shape `Status eq 'Published'` with
/// a sort): the keyset continuation over the sorted order enumerates the whole
/// matching set across pages with no gaps or repeats, and non-matching rows are
/// never returned.
#[tokio::test]
async fn filtered_ordered_pagination_matches_single_read() {
    let (state, store, db_path) = turso_state("paging-filtered").await;
    let tenant = TenantId::default();

    // 9 orders in the "keep" bucket with assorted Totals (ties included), plus 3
    // in another bucket that the filter must exclude on every page.
    let totals = [30_u64, 10, 20, 10, 40, 25, 10, 35, 5];
    for (index, total) in totals.iter().enumerate() {
        seed_order(
            &state,
            &store,
            &tenant,
            &format!("keep-{index:02}"),
            serde_json::json!({ "Bucket": "keep", "Total": total }),
        )
        .await;
    }
    for index in 0..3usize {
        seed_order(
            &state,
            &store,
            &tenant,
            &format!("drop-{index:02}"),
            serde_json::json!({ "Bucket": "other", "Total": 99 }),
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 2,
        max_entities: 100,
    };
    let base = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Bucket".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("keep".to_string()))),
        }),
        orderby: Some(vec![OrderByClause {
            property: "Total".to_string(),
            direction: OrderDirection::Asc,
        }]),
        ..QueryOptions::default()
    };

    let paged = walk_pages(&state, &tenant, &security_ctx, &base, budget).await;
    let whole = single_read(&state, &tenant, &security_ctx, &base, budget).await;

    assert_eq!(whole.len(), 9, "only the nine keep-bucket orders match");
    assert_eq!(paged, whole, "paged union equals the sorted single read");
    assert!(
        paged.iter().all(|id| id.starts_with("keep-")),
        "no filtered-out rows leaked"
    );

    let _ = std::fs::remove_file(db_path);
}

/// ARN-97: a `$select` projected list-read returns exactly the same entity set
/// as the canonical (no-`$select`) read of the same `$filter`, page for page —
/// only the field shape differs. The set includes an entity whose `Notes` value
/// exceeds the 2000-byte field-index cap (so it is absent from the EAV index),
/// which previously stressed the projection path.
#[tokio::test]
async fn projected_read_matches_canonical_read_entity_set() {
    let (state, store, db_path) = turso_state("paging-select-invariant").await;
    let tenant = TenantId::default();

    let oversized = "n".repeat(4096); // exceeds the 2000-byte indexable cap
    for index in 0..6usize {
        // One entity carries an un-indexable Notes value; all share Bucket=keep.
        let notes = if index == 3 {
            serde_json::json!(oversized)
        } else {
            serde_json::json!("short")
        };
        seed_order(
            &state,
            &store,
            &tenant,
            &format!("ord-{index:02}"),
            serde_json::json!({ "Bucket": "keep", "Notes": notes }),
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 2,
        max_entities: 100,
    };
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Bucket".to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("keep".to_string()))),
    };

    let canonical = QueryOptions {
        filter: Some(filter.clone()),
        ..QueryOptions::default()
    };
    let projected = QueryOptions {
        filter: Some(filter),
        select: Some(vec!["Id".to_string(), "Bucket".to_string()]),
        ..QueryOptions::default()
    };

    let canonical_ids = walk_pages(&state, &tenant, &security_ctx, &canonical, budget).await;
    let projected_ids = walk_pages(&state, &tenant, &security_ctx, &projected, budget).await;

    assert_eq!(
        canonical_ids.len(),
        6,
        "all six keep orders are visible canonically"
    );
    assert_eq!(
        projected_ids, canonical_ids,
        "the projected read must return the same entity set as the canonical read"
    );
    assert!(
        canonical_ids.contains(&"ord-03".to_string()),
        "the entity with an un-indexable field value must be present"
    );

    let _ = std::fs::remove_file(db_path);
}

/// The continuation must also enumerate the whole set on the deterministic sim
/// store (no query-plane backend → the authoritative source-cursor path), and do
/// so identically under every seed. This is the backend the DST harness runs.
#[tokio::test]
async fn sim_pagination_is_deterministic_across_seeds() {
    let mut reference: Option<Vec<String>> = None;
    for seed in 0..16u64 {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let events = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let mut state = build_order_state("paging-sim");
        state.set_storage_stack(StorageStack::new(
            BackendLabel::Sim,
            events,
            None,
            None,
            None,
            None,
            None, // no query plane: reads go through the authoritative source cursor
            None,
            None,
            None,
            None,
        ));
        let tenant = TenantId::default();
        let agent_ctx = AgentContext::for_service("paging-sim");
        for index in 0..7usize {
            state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &format!("ord-{index:02}"),
                    "Create",
                    serde_json::json!({}),
                    &agent_ctx,
                )
                .await
                .expect("create order");
        }

        let security_ctx = SecurityContext::system();
        let budget = QueryPlaneReadBudget {
            default_page_size: 2,
            max_entities: 100,
        };
        let base = QueryOptions::default();
        let paged = walk_pages(&state, &tenant, &security_ctx, &base, budget).await;
        let whole = single_read(&state, &tenant, &security_ctx, &base, budget).await;

        assert_eq!(
            paged, whole,
            "seed {seed}: paged union must equal one big read"
        );
        assert_eq!(paged.len(), 7, "seed {seed}: all seven orders enumerated");
        match &reference {
            Some(expected) => assert_eq!(
                &paged, expected,
                "seed {seed}: pagination not deterministic"
            ),
            None => reference = Some(paged),
        }
    }
}
