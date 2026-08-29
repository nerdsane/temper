use super::*;

#[test]
fn idempotency_actor_key_matches_actor_persistence_id_shape() {
    let tenant = TenantId::new("acme");

    assert_eq!(
        idempotency_actor_key(&tenant, "WorkCycle", "wc-1", None),
        "acme:WorkCycle:wc-1"
    );
}

#[tokio::test]
async fn collection_workflow_modes_return_stable_odata_conflicts() {
    for code in ["CollectionWorkflowDraining", "CollectionWorkflowDisabled"] {
        let response = collection_workflow_conflict_response(code);
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("OData conflict body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("OData conflict JSON");
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["message"], code);
    }
}
