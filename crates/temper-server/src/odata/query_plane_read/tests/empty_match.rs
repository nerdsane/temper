//! An exact-match read that finds nothing re-reads only the rows the field
//! index cannot answer for (ARN-89), not every row that holds no value for the
//! filtered field. Katagami's art-style notice asks `creator_sub eq '<sub>'` of
//! 582 ArtStyles on every connector call; 527 have no `creator_sub`, and each
//! empty answer loaded all 527 in full (about 600 ms).

use super::*;

fn notes_eq(value: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Notes".to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(value.to_string()))),
    }
}

/// Six Orders are projected from their latest event, none with Notes. A read
/// for `Notes eq 'late'` finds nothing and loads none of them: each catalog row
/// and its field index hold the Order's committed fields, so the empty page is
/// the answer. A row the projection has not reached is still re-read, which
/// `exact_match_read_is_consistent_when_projection_queued` holds.
#[tokio::test]
async fn an_empty_match_loads_no_row_the_field_index_answers_for() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-empty-match-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("query-plane-empty-match");

    let writer = build_order_state_with_store("query-plane-empty-match-writer", &store);
    let ids: Vec<String> = (0..6).map(|index| format!("ord-{index:02}")).collect();
    for entity_id in &ids {
        writer
            .dispatch_tenant_action(
                &tenant,
                "Order",
                entity_id,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        let committed = writer
            .get_tenant_entity_state(&tenant, "Order", entity_id)
            .await
            .expect("read the created order")
            .state
            .sequence_nr;
        upsert_order_projection(
            &store,
            &tenant,
            entity_id,
            serde_json::json!({ "Status": "Draft" }),
            committed,
        )
        .await;
    }

    // A fresh reader, as after a restart.
    let mut state = build_order_state("query-plane-empty-match-reader");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(notes_eq("late")),
        top: Some(1),
        ..QueryOptions::default()
    };
    let result = match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("an empty exact-match read answers, not errors"),
    };

    assert!(result.entities.is_empty(), "no Order has Notes");
    assert_eq!(
        (
            result.telemetry.coverage.missing,
            result.telemetry.materialized_count
        ),
        (0, 0),
        "every Order's projection is current, so none is re-read: {:?}",
        result.telemetry
    );

    let _ = std::fs::remove_file(db_path);
}
