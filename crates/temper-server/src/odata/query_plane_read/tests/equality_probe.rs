//! Regression for complete equality reconciliation beyond the candidate probe.
use super::equality_delta::notes;
use super::*;
#[tokio::test]
async fn equality_delta_eleven_matches_and_pending_nonmatch_answer_top_one() {
    for match_count in [11, 270] {
        let dir = tempfile::tempdir().unwrap();
        let store = TursoEventStore::new(
            &format!("file:{}", dir.path().join("probe.db").display()),
            None,
        )
        .await
        .unwrap();
        let state = build_order_state_with_store("delta-probe", &store);
        let tenant = TenantId::default();
        for i in 0..=match_count {
            let id = format!("ord-{i:03}");
            let created = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &id,
                    "AddItem",
                    serde_json::json!({"ProductId": "p", "Quantity": 1,
                "Notes": if i == match_count { "noise" } else { "needle" }}),
                    &AgentContext::for_service("delta-probe"),
                )
                .await
                .unwrap();
            assert!(created.success);
        }
        store
            .remove_query_projection(tenant.as_str(), "Order", &format!("ord-{match_count:03}"))
            .await
            .unwrap();
        let options = QueryOptions {
            top: Some(1),
            skip: (match_count > 11).then_some(match_count - 1),
            ..notes("needle")
        };
        let security = SecurityContext::system();
        for caught_up in [false, true] {
            if caught_up {
                let updated = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Order",
                        &format!("ord-{match_count:03}"),
                        "AddItem",
                        serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": "noise"}),
                        &AgentContext::for_service("delta-probe"),
                    )
                    .await
                    .unwrap();
                assert!(updated.success);
            }
            for count in [None, Some(true)] {
                let options = QueryOptions {
                    count,
                    ..options.clone()
                };
                let result = read_entity_set_page(QueryPlaneReadRequest {
                    state: &state,
                    tenant: &tenant,
                    security_ctx: &security,
                    entity_type: "Order",
                    entity_set_name: "Orders",
                    query_options: &options,
                    budget: QueryPlaneReadBudget {
                        default_page_size: 1,
                        max_entities: 1,
                    },
                })
                .await
                .unwrap_or_else(|error| {
                    let detail = match error {
                        QueryPlaneReadError::QueryTooLarge { telemetry } => {
                            format!("QueryTooLarge: {telemetry:?}")
                        }
                        QueryPlaneReadError::ReadUnavailable => "ReadUnavailable".to_string(),
                        _ => "unexpected query error".to_string(),
                    };
                    panic!(
                        "{match_count} matches, caught_up {caught_up}, count {count:?}: {detail}"
                    )
                });
                assert_eq!(result.count, count.map(|_| match_count));
                assert_eq!(result.entities.len(), 1);
                assert_eq!(
                    result.entities[0]["entity_id"],
                    if match_count == 11 {
                        "ord-000".to_string()
                    } else {
                        format!("ord-{:03}", match_count - 1)
                    }
                );
                if !caught_up {
                    assert_eq!(result.telemetry.candidate_count, match_count + 1);
                }
            }
        }
    }
}
