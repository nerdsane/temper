use super::*;

#[tokio::test]
async fn bootstrap_dispatch_requires_its_dedicated_artifact_grant() {
    let invocation = invocation(BTreeSet::new(), SecurityContext::system());
    let response = call_bootstrap(
        &invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-denied".into(),
            idempotency_key: "bootstrap-denied".into(),
            activation_request_id: "activation-denied".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: "018f1f80-7b2d-7000-8000-000000000079".into(),
            initial_fields: serde_json::Map::new(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Error { error } = response else {
        panic!("lookalike schema grants must not authorize bootstrap")
    };
    assert_eq!(error.code, "authorization_denied");
}

#[tokio::test]
async fn bootstrap_dispatch_grant_is_bound_to_the_exact_entity_type() {
    let original = invocation(
        BTreeSet::from([DataOperationKind::SchemaBootstrapDispatch]),
        SecurityContext::system(),
    );
    let mut authority = original.authority.clone();
    authority.binding.grant.entities[0].entity_type = "Temper.Example.Other".into();
    let invocation = ApplicationDataInvocation::new(original.state.clone(), authority);
    let response = call_bootstrap(
        &invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-wrong-entity".into(),
            idempotency_key: "bootstrap-wrong-entity".into(),
            activation_request_id: "activation-wrong-entity".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: "018f1f80-7b2d-7000-8000-000000000083".into(),
            initial_fields: serde_json::Map::new(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Error { error } = response else {
        panic!("an entity-mismatched bootstrap grant must be denied")
    };
    assert_eq!(error.code, "authorization_denied");
}
