use super::*;
use temper_runtime::ActorSystem;

#[tokio::test]
async fn endpoint_hosts_use_engine_clients_but_keep_streams_and_authorization_per_invocation() {
    let state = crate::state::ServerState::from_registry(
        ActorSystem::new("endpoint-client-reuse"),
        crate::registry::SpecRegistry::new(),
    );
    let tenant = TenantId::default();
    let context: WasmInvocationContext = serde_json::from_value(json!({
        "tenant": tenant.as_str(),
        "entity_type": "HttpEndpoint",
        "entity_id": "endpoint-client-reuse",
        "trigger_action": "Handle",
        "trigger_params": {},
        "entity_state": {}
    }))
    .unwrap();
    assert!(state.wasm_engine.http_clients().is_empty());
    for payload in [b"first".to_vec(), b"second".to_vec()] {
        let streams = Arc::new(temper_wasm::http_stream::HttpStreamRegistry::new());
        let (writer, reader) = streams.create_pair().await;
        let host = authorized_http_endpoint_host(
            &state,
            &tenant,
            "endpoint-client-reuse",
            &context,
            Arc::clone(&streams),
        )
        .unwrap();
        assert_eq!(state.wasm_engine.http_clients().len(), 1);
        host.http_stream_try_write(writer, payload.clone())
            .await
            .unwrap();
        assert_eq!(streams.read(reader).await.unwrap(), payload);
        assert!(host.get_secret("other-tenant-secret").is_err());
        let error = host
            .http_call("GET", "http://127.0.0.1:9/denied", &[], "")
            .await
            .expect_err("cached transport must not bypass Cedar");
        assert!(error.contains("authorization denied"), "{error}");
    }
}
