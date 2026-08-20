//! Deterministic simulation of the read/projection plane under projection lag
//! (ADR-0153, ARN-68).
//!
//! This is a TRUE DST, not a unit/integration/e2e test: it runs the **real**
//! read planner (`read_entity_set_page`) under
//! `install_deterministic_context(seed)` across many seeds, against deterministic
//! in-memory backends (`SimEventStore` for the journal + key index, `SimQueryPlane`
//! for the catalog), with a **fault injected** — the async query projection lags,
//! modeled as "the field-index pushdown is unavailable" (`query_field_index_page`
//! returns `None`). That is the production trigger for the 413: native pushdown
//! can't narrow, so the planner falls back to the authoritative scan, which trips
//! the read budget at scale.
//!
//! The simulation reproduces the 413 deterministically (no key, or a non-key
//! filter → QueryTooLarge) and proves the co-committed declared-key index
//! eliminates it (keyed filter → bounded candidate, no 413) — under every seed.
//! This is the plane the existing DST did not cover; it is why this class of bug
//! (413, read-after-write) kept escaping to production.

use super::*;
use crate::storage::{
    BoxedEventStore, EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage, QueryPlaneStore,
    QueryProjectionFieldsRow, StorageStack,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_store_sim::SimEventStore;

const DST_SEEDS: u64 = 64;

/// Deterministic in-memory query plane. Models the production async projection:
/// the catalog is populated by `upsert_projection`, but **field-index pushdown is
/// reported unavailable** (`query_field_index_page` → `None`) to simulate the
/// projection lagging behind the journal — the exact condition that makes the
/// real planner fall back to the authoritative scan (and 413 at scale).
#[derive(Default)]
struct SimQueryPlane {
    // (entity_type, entity_id) -> catalog row
    catalog: Mutex<BTreeMap<(String, String), EntityCatalogRow>>,
}

#[async_trait]
impl QueryPlaneStore for SimQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.catalog.lock().unwrap().insert(
            (entity_type.to_string(), entity_id.to_string()),
            EntityCatalogRow {
                entity_id: entity_id.to_string(),
                status: status.to_string(),
                fields: fields.clone(),
                state: Some(state.clone()),
                sequence_nr,
            },
        );
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.catalog
            .lock()
            .unwrap()
            .remove(&(entity_type.to_string(), entity_id.to_string()));
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        // Projection lag: the field index cannot narrow this read.
        Ok(None)
    }

    async fn query_field_index_page(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
        _order_by: &[QueryFieldIndexOrder],
        _skip: usize,
        _top: usize,
        _include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        // The injected fault: native pushdown is unavailable (projection lag),
        // so the planner must fall back to the authoritative scan.
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        let catalog = self.catalog.lock().unwrap();
        let rows = entity_ids
            .iter()
            .filter_map(|id| catalog.get(&(entity_type.to_string(), id.clone())).cloned())
            .collect();
        Ok(Some(rows))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "dst-lag".to_string(),
        },
    }
}

fn doc_key_hash(workspace: &str, path: &str) -> String {
    crate::key_index::canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &serde_json::json!({ "WorkspaceId": workspace, "Path": path })
            .as_object()
            .unwrap()
            .clone(),
    )
    .expect("complete key")
}

fn eq_filter(ws: &str, path: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(ws.to_string()))),
        }),
        op: BinaryOperator::And,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Path".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(path.to_string()))),
        }),
    }
}

/// Build a sim-backed ServerState with the keyed Order table installed.
fn sim_state(seed: u64, qp: std::sync::Arc<SimQueryPlane>) -> (ServerState, BoxedEventStore) {
    let events = BoxedEventStore::new(SimEventStore::no_faults(seed));
    let mut state = build_order_state("dst-projection-lag");
    state.set_storage_stack(StorageStack::new(
        crate::storage::BackendLabel::Sim,
        events.clone(),
        None,
        None,
        None,
        None,
        Some(qp),
        None,
        None,
        None,
        None,
    ));
    state.transition_tables = std::sync::Arc::new(
        [(
            "Order".to_string(),
            std::sync::Arc::new(temper_jit::table::TransitionTable::from_ioa_source(
                ORDER_IOA,
            )),
        )]
        .into_iter()
        .collect(),
    );
    (state, events)
}

/// THE DST. Under projection lag (pushdown unavailable) and a workspace larger
/// than the read budget:
///   * a read with NO usable keyed access path falls back to the authoritative
///     scan and returns **413 QueryTooLarge** — reproducing the production bug
///     deterministically;
///   * the SAME read, when its `$filter` is exactly the declared key and the
///     co-committed key row exists, resolves to a **bounded single candidate** and
///     returns **200** — the elimination.
/// Holds under every seed.
#[tokio::test]
async fn dst_projection_lag_413_eliminated_by_keyed_index() {
    for seed in 0..DST_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let qp = std::sync::Arc::new(SimQueryPlane::default());
        let (state, events) = sim_state(seed, qp.clone());
        let tenant = TenantId::default();

        let ws = "ws-lag";
        let target_path = "/proofs/keyed-under-lag.txt";
        let target_id = format!("ord-target-{seed}");

        // A workspace larger than the read budget. Noise lives in the journal only
        // (so list_entity_ids sees them) — the projection lags for everyone.
        for i in 0..16usize {
            let pid = format!("{tenant}:Order:noise-{seed}-{i:02}");
            events
                .append(&pid, 0, &[envelope("Create", serde_json::json!({}))])
                .await
                .unwrap();
        }

        // The target: co-commit the declared key row with the journal (immediate,
        // no lag), and project it to the catalog so it can be materialized once
        // the keyed path bounds the candidate set.
        let target_pid = format!("{tenant}:Order:{target_id}");
        events
            .append_with_keys(
                &target_pid,
                0,
                &[envelope(
                    "Create",
                    serde_json::json!({ "WorkspaceId": ws, "Path": target_path }),
                )],
                &[EntityKeyRow {
                    key_name: "ws_path".to_string(),
                    key_hash: doc_key_hash(ws, target_path),
                }],
            )
            .await
            .unwrap();
        qp.upsert_projection(
            tenant.as_str(),
            "Order",
            &target_id,
            "Created",
            &serde_json::json!({ "Id": target_id, "WorkspaceId": ws, "Path": target_path }),
            &serde_json::json!({}),
            1,
        )
        .await
        .unwrap();

        let security_ctx = SecurityContext::system();
        // A budget smaller than the workspace: scan_candidate_budget = max(10*1, 1).
        let budget = QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        };

        // RED (reproduce the prod 413): a present-but-non-keyed read shape. Use a
        // filter on a non-declared property; pushdown is unavailable (lag), so the
        // planner scans the whole workspace (> budget) -> QueryTooLarge.
        let non_key = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Notes".to_string())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("x".to_string()))),
            }),
            ..QueryOptions::default()
        };
        let red = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &non_key,
            budget,
        })
        .await;
        assert!(
            matches!(red, Err(QueryPlaneReadError::QueryTooLarge { .. })),
            "seed {seed}: under projection lag a non-keyed read at scale must 413 (reproduces the prod bug)"
        );

        // RED, filter held constant: the SAME keyed $filter shape, but for a key
        // that has NO co-committed row (pre-backfill / absent). The keyed lookup
        // misses, falls back to the scan -> 413. This isolates the key row as the
        // single variable behind the before/after.
        let keyed_absent = QueryOptions {
            filter: Some(eq_filter(ws, "/no-such-path.txt")),
            ..QueryOptions::default()
        };
        let red_absent = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &keyed_absent,
            budget,
        })
        .await;
        assert!(
            matches!(red_absent, Err(QueryPlaneReadError::QueryTooLarge { .. })),
            "seed {seed}: same keyed filter shape with NO key row must still 413 (pre-backfill scan fallback)"
        );

        // GREEN (the fix): the SAME workspace + budget + lag, but the $filter is
        // exactly the declared key and the co-committed key row exists. The keyed
        // fast path bounds the candidate set to the single id -> no scan -> 200.
        let keyed = QueryOptions {
            filter: Some(eq_filter(ws, target_path)),
            ..QueryOptions::default()
        };
        let green = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &keyed,
            budget,
        })
        .await;
        let green = match green {
            Ok(r) => r,
            Err(_) => panic!("seed {seed}: keyed read under lag must not 413"),
        };
        assert!(
            green.telemetry.candidate_count <= 1,
            "seed {seed}: keyed read must bound the candidate set (no scan); candidate_count={}",
            green.telemetry.candidate_count
        );
    }
}
