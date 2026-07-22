use super::*;

mod catalog_fault;
mod composite_events;
mod fault_paths;
mod projection_race;
mod source_transitions;

#[tokio::test]
async fn incomplete_key_scan_replays_journal_not_stale_resident_actor() {
    let (_guard, _clock, _ids) = install_deterministic_context(249);
    let tenant = TenantId::default();
    let workspace = "ws-incomplete-stale-actor";
    let stale_path = "/before";
    let current_path = "/after";
    let entity_id = "ord-incomplete-stale-resident";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("incomplete-key-stale-actor");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({
                "Id": entity_id,
                "WorkspaceId": workspace,
                "Path": stale_path,
            }),
        )
        .await
        .expect("spawn resident actor at sequence one");
    let sequence = current_sequence(&store, &tenant, "Order", entity_id).await;
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        sequence,
        &[field_update_event(
            &persistence_id,
            current_path,
            "external-rename-before-coverage",
        )],
        &[key_row(workspace, current_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
            snapshot_source: Default::default(),
        },
    )
    .await
    .expect("commit rename outside the resident actor");

    assert!(
        EventStore::key_index_backfilled_types(&store, tenant.as_str())
            .await
            .expect("read incomplete coverage")
            .is_empty(),
        "the read must take the incomplete-coverage authoritative scan"
    );
    let stale = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("resident actor remains readable");
    assert_eq!(stale.state.fields["Path"], stale_path);

    let security_ctx = SecurityContext::system();
    let read_path = |path: &str| QueryOptions {
        filter: Some(ws_path_filter(workspace, path)),
        ..QueryOptions::default()
    };
    let old_options = read_path(stale_path);
    let old_result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &old_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await;
    let old_result = match old_result {
        Ok(result) => result,
        Err(_) => panic!("old-key authoritative scan must remain available"),
    };
    assert!(
        old_result.entities.is_empty(),
        "an incomplete-key scan must not return the stale resident actor at sequence {sequence}"
    );

    let current_options = read_path(current_path);
    let current_result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &current_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await;
    let current_result = match current_result {
        Ok(result) => result,
        Err(_) => panic!("current-key authoritative scan must remain available"),
    };
    assert_eq!(current_result.entities.len(), 1);
    assert_eq!(current_result.entities[0]["entity_id"], entity_id);
    assert_eq!(current_result.entities[0]["fields"]["Path"], current_path);
    assert_eq!(current_result.entities[0]["sequence_nr"], sequence + 1);
}
