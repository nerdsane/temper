use super::*;
use temper_runtime::tenant::TenantId;

#[tokio::test]
async fn historical_forged_mirrors_repair_equally_from_journal_and_snapshot_tail() {
    let source = SimEventStore::no_faults(549);
    let live = state(source.clone(), false);
    create_and_advance(&live, "acme").await;
    let (_, body) = request(&live, "acme", "PATCH", ENTITY, json!({"Notes":"writable"})).await;
    let mut snapshot = live
        .get_tenant_entity_state(&TenantId::new("acme"), "Order", "owned")
        .await
        .unwrap()
        .state;
    assert_eq!(body["sequence_nr"], 3);
    let mut events = source.read_events("acme:Order:owned", 0).await.unwrap();
    let forged = json!({
        "units":900,"Units":901,"approved":false,"reviewers":["forged"],
        "pending_count":902,"Notes":"writable"
    });
    // Model a journal and snapshot written by the vulnerable generic-write path.
    events[2].payload["params"] = forged.clone();
    for (key, value) in forged.as_object().unwrap() {
        snapshot.fields[key] = value.clone();
    }
    for replace in [false, true] {
        let historical_type = if replace {
            "FieldsReplaced"
        } else {
            "FieldsUpdated"
        };
        events[2].event_type = historical_type.into();
        events[2].payload["action"] = json!(historical_type);
        for with_tail in [false, true] {
            let mut history = events.clone();
            if with_tail {
                let mut tail = events[2].clone();
                tail.sequence_nr = 4;
                tail.event_type = "FieldsReplaced".into();
                tail.payload["action"] = json!("FieldsReplaced");
                tail.payload["params"] = json!({"Notes":"writable"});
                history.push(tail);
            }
            let mut recovered = Vec::new();
            for with_snapshot in [false, true] {
                let store = SimEventStore::no_faults(550);
                store.append("acme:Order:owned", 0, &history).await.unwrap();
                if with_snapshot {
                    store
                        .save_snapshot(
                            "acme:Order:owned",
                            3,
                            &serde_json::to_vec(&snapshot).unwrap(),
                        )
                        .await
                        .unwrap();
                }
                let server = state(store.clone(), false);
                let entity = server
                    .get_tenant_entity_state(&TenantId::new("acme"), "Order", "owned")
                    .await
                    .unwrap();
                let view = serde_json::to_value(&entity.state).unwrap();
                assert_owned_view(&view);
                recovered.push(entity.state.fields);
                server
                    .authz
                    .reload_tenant_policies(
                        "acme",
                        r#"
                    permit(principal, action == Action::"update", resource)
                    when { resource.units == 900 };
                "#,
                    )
                    .unwrap();
                let (status, body) = request(
                    &server,
                    "acme",
                    "PATCH",
                    ENTITY,
                    json!({"Notes":"unauthorized"}),
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "legacy attacker mirror authorized PATCH: {body}"
                );
                assert_eq!(
                    store
                        .read_events("acme:Order:owned", 0)
                        .await
                        .unwrap()
                        .len(),
                    history.len()
                );
            }
            assert_eq!(
                recovered[0], recovered[1],
                "snapshot+tail differs from full fold"
            );
        }
    }
}

#[tokio::test]
async fn legacy_created_payload_cannot_seed_authoritative_mirrors() {
    let source = SimEventStore::no_faults(551);
    let live = state(source.clone(), false);
    request(
        &live,
        "acme",
        "POST",
        "/tdata/Orders",
        json!({"id":"owned"}),
    )
    .await;
    let mut events = source.read_events("acme:Order:owned", 0).await.unwrap();
    events[0]
        .payload
        .as_object_mut()
        .unwrap()
        .remove("initial_values");
    events[0].payload["params"] =
        json!({"units":900,"approved":true,"reviewers":["forged"],"Notes":"writable"});
    let store = SimEventStore::no_faults(552);
    store.append("acme:Order:owned", 0, &events).await.unwrap();
    let cold = state(store, false);
    let recovered = cold
        .get_tenant_entity_state(&TenantId::new("acme"), "Order", "owned")
        .await
        .unwrap();
    assert!(recovered.state.counters.is_empty());
    assert!(recovered.state.booleans.is_empty());
    assert!(recovered.state.lists.is_empty());
    for name in ["units", "approved", "reviewers"] {
        assert!(recovered.state.fields.get(name).is_none());
    }
    assert_eq!(recovered.state.fields["Notes"], "writable");
}
