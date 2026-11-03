//! The built-in `http_fetch` module the server embeds (`crates/temper-wasm/modules/http_fetch.wasm`, built from
//! `wasm-modules/http-fetch` by `scripts/build-http-fetch-wasm.sh`). Its request goes to the url, method and headers
//! of the spec's integration config; action params and entity fields are the request's data only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use temper_wasm::{
    StreamRegistry, WasmEngine, WasmHost, WasmInvocationContext, WasmResourceLimits,
};

const HTTP_FETCH_WASM: &[u8] = include_bytes!("../modules/http_fetch.wasm");

/// One request the module asked the host to send.
#[derive(Debug, Clone, PartialEq)]
struct SentRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Answers every request with `{"ok":true}` and keeps what it was asked to send.
#[derive(Default)]
struct RecordingHost {
    sent: Mutex<Vec<SentRequest>>,
}

#[async_trait]
impl WasmHost for RecordingHost {
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        self.sent.lock().expect("lock").push(SentRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: headers.to_vec(),
            body: body.to_string(),
        });
        Ok((200, r#"{"ok":true}"#.to_string()))
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("secret not found: {key}"))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Err("not used".to_string())
    }

    fn log(&self, _level: &str, _message: &str) {}
}

fn context(
    config: &[(&str, &str)],
    trigger_params: serde_json::Value,
    entity_state: serde_json::Value,
) -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "default".to_string(),
        entity_type: "Item".to_string(),
        entity_id: "item-1".to_string(),
        trigger_action: "Fetch".to_string(),
        wasm_module: Some("http_fetch".to_string()),
        trigger_params,
        entity_state,
        agent_id: None,
        session_id: None,
        integration_config: config
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
        trigger_principal: None,
    }
}

async fn run(ctx: &WasmInvocationContext) -> (temper_wasm::WasmInvocationResult, Vec<SentRequest>) {
    let engine = WasmEngine::new().expect("engine");
    let hash = engine.compile_and_cache(HTTP_FETCH_WASM).expect("compile");
    let host = Arc::new(RecordingHost::default());
    let result = engine
        .invoke(
            &hash,
            ctx,
            host.clone(),
            &WasmResourceLimits::default(),
            Arc::new(RwLock::new(StreamRegistry::default())),
        )
        .await
        .expect("invoke");
    let sent = host.sent.lock().expect("lock").clone();
    (result, sent)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_goes_to_the_configured_url_whatever_the_params_and_fields_name() {
    let elsewhere =
        serde_json::json!({ "url": "http://127.0.0.2:9/other", "method": "PUT", "headers": "[]" });
    let ctx = context(
        &[("url", "http://127.0.0.1:9/configured"), ("method", "POST")],
        serde_json::json!({
            "note": "n1",
            "integration_config": elsewhere,
            "nested": { "integration_config": elsewhere },
        }),
        serde_json::json!({ "fields": { "integration_config": elsewhere } }),
    );

    let (result, sent) = run(&ctx).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].url, "http://127.0.0.1:9/configured");
    // The params are the request body, as sent.
    let body: serde_json::Value = serde_json::from_str(&sent[0].body).expect("JSON body");
    assert_eq!(body, ctx.trigger_params);
    assert_eq!(
        result.callback_params["response"],
        serde_json::json!({ "ok": true })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_get_puts_the_params_in_the_query_of_the_configured_url() {
    let ctx = context(
        &[("url", "http://127.0.0.1:9/configured"), ("method", "GET")],
        serde_json::json!({ "q": "x" }),
        serde_json::json!({}),
    );

    let (result, sent) = run(&ctx).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(sent[0].url, "http://127.0.0.1:9/configured?q=x");
    assert_eq!(sent[0].body, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_headers_are_sent_as_an_object_or_as_pairs() {
    for headers in [r#"{"X-Dummy":"marker-1"}"#, r#"[["X-Dummy","marker-1"]]"#] {
        let ctx = context(
            &[
                ("url", "http://127.0.0.1:9/configured"),
                ("method", "POST"),
                ("headers", headers),
            ],
            serde_json::json!({ "headers": { "X-Other": "from-params" } }),
            serde_json::json!({}),
        );

        let (result, sent) = run(&ctx).await;

        assert!(result.success, "{headers}: {:?}", result.error);
        assert_eq!(
            sent[0].headers,
            vec![("X-Dummy".to_string(), "marker-1".to_string())],
            "{headers}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_configured_headers_fail_without_a_request() {
    let ctx = context(
        &[
            ("url", "http://127.0.0.1:9/configured"),
            ("headers", "X-Dummy: marker-1"),
        ],
        serde_json::json!({}),
        serde_json::json!({}),
    );

    let (result, sent) = run(&ctx).await;

    assert!(!result.success);
    assert!(sent.is_empty(), "{sent:?}");
}
