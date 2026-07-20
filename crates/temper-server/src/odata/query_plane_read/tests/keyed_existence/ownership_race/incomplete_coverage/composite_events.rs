use super::*;
use crate::entity_actor::EntityEvent;
use temper_runtime::persistence::COMPOSITE_EVENT_TYPE;

fn composite_named_domain_event(
    persistence_id: &str,
    entity_id: &str,
    workspace: &str,
    path: &str,
) -> PersistenceEnvelope {
    let timestamp = sim_now();
    let event = EntityEvent {
        action: COMPOSITE_EVENT_TYPE.to_string(),
        from_status: "Draft".to_string(),
        to_status: "Draft".to_string(),
        timestamp,
        params: serde_json::json!({
            "Id": entity_id,
            "WorkspaceId": workspace,
            "Path": path,
        }),
        idempotency_key: Some("domain-composite-name".to_string()),
    };
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: COMPOSITE_EVENT_TYPE.to_string(),
        payload: serde_json::to_value(event).expect("serialize domain event"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

#[tokio::test]
async fn composite_event_type_only_skips_the_runtime_audit_schema() {
    let (_guard, _clock, _ids) = install_deterministic_context(262);
    let tenant = TenantId::default();
    let workspace = "ws-domain-composite";
    let path = "/domain-action";
    let entity_id = "ord-domain-composite";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("domain-composite-event-name");

    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[composite_named_domain_event(
            &persistence_id,
            entity_id,
            workspace,
            path,
        )],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed domain event whose legal action name matches the audit record type");

    let result = match super::source_transitions::read_path(&state, &tenant, workspace, path).await
    {
        Ok(result) => result,
        Err(_) => panic!("domain event must remain replayable"),
    };
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], entity_id);
    assert_eq!(result.entities[0]["fields"]["Path"], path);
}

#[tokio::test]
async fn malformed_composite_audit_record_is_not_silently_authoritative() {
    let (_guard, _clock, _ids) = install_deterministic_context(263);
    let tenant = TenantId::default();
    let workspace = "ws-malformed-composite";
    let path = "/malformed-audit";
    let entity_id = "ord-malformed-composite";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("malformed-composite-audit");
    let timestamp = sim_now();

    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[PersistenceEnvelope {
            sequence_nr: 0,
            event_type: COMPOSITE_EVENT_TYPE.to_string(),
            payload: serde_json::json!({"corrupt": true}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.clone(),
            },
        }],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed malformed composite audit record");

    let result = super::source_transitions::read_path(&state, &tenant, workspace, path).await;
    assert!(
        matches!(result, Err(QueryPlaneReadError::KeyOwnershipUnstable)),
        "strict authoritative replay must reject an undecodable composite record"
    );
}
