//! Exercise the guest ABI and the warning emitted when stream setup is refused.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, RwLock};

use temper_wasm::{
    ProductionWasmHost, WasmEngine, WasmInvocationContext, WasmResourceLimits,
    stream::StreamRegistry,
};

#[derive(Clone)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn stream_setup_warning_keeps_host_and_path_without_secrets() {
    // This integration binary has one test. A global subscriber also observes
    // the engine's invocation thread, where the guest calls the host ABI.
    let log = CapturedLog(Arc::new(Mutex::new(Vec::new())));
    let writer = log.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || writer.clone())
            .finish(),
    )
    .expect("install log capture");

    let key = "0123456789abcdef0123456789abcdef";
    let engine = WasmEngine::new().expect("engine");
    for url in [
        format!("https://h/build?account=user@{key}"),
        format!("https://user:password@h/build#user@{key}"),
        "https://h/0123456789abcdef~fedcba9876543210/build".to_string(),
        "https://h/0123456789abcdef.fedcba9876543210/build".to_string(),
    ] {
        let wat = format!(
            r#"(module
                (import "env" "host_http_stream_begin_outbound" (func $begin
                    (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 2048) "TRACE")
                (data (i32.const 2056) "{url}")
                (func (export "run") (param i32 i32) (result i32)
                    (if (i32.ne
                        (call $begin (i32.const 2048) (i32.const 5)
                            (i32.const 2056) (i32.const {url_len})
                            (i32.const 0) (i32.const 0)
                            (i32.const 4096) (i32.const 4100))
                        (i32.const -4))
                        (then unreachable))
                    (i32.const 0)))"#,
            url_len = url.len(),
        );
        let hash = engine
            .compile_and_cache(wat.as_bytes())
            .expect("compile guest");
        engine
            .invoke(
                &hash,
                &WasmInvocationContext {
                    tenant: "test".into(),
                    entity_type: "Test".into(),
                    entity_id: "1".into(),
                    trigger_action: "Run".into(),
                    wasm_module: Some("stream_warning_test".into()),
                    trigger_params: serde_json::Value::Null,
                    entity_state: serde_json::Value::Null,
                    agent_id: None,
                    session_id: None,
                    integration_config: BTreeMap::new(),
                    trace_id: String::new(),
                    workflow_root_entity_type: None,
                    workflow_root_entity_id: None,
                    workflow_run_id: None,
                    http_request: None,
                    trigger_principal: None,
                },
                Arc::new(ProductionWasmHost::new(BTreeMap::new())),
                &WasmResourceLimits::default(),
                Arc::new(RwLock::new(StreamRegistry::default())),
            )
            .await
            .expect("guest observes refused stream setup");
    }

    let bytes = log.0.lock().expect("log capture lock").clone();
    let output = String::from_utf8(bytes).expect("UTF-8 logs");
    let warnings: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).expect("structured log"))
        .filter(|event: &serde_json::Value| {
            event["fields"]["message"] == "WASM outbound HTTP stream begin failed"
        })
        .collect();
    assert_eq!(
        warnings.len(),
        4,
        "must observe each stream refusal: {output}"
    );
    for (warning, expected_url) in warnings.iter().zip([
        "h/build",
        "h/build",
        "h/<redacted>/build",
        "h/<redacted>/build",
    ]) {
        assert_eq!(warning["level"], "WARN");
        assert_eq!(warning["fields"]["url"], expected_url);
        assert_eq!(warning["fields"]["http_method"], "TRACE");
        assert!(
            warning["fields"]["error"]
                .as_str()
                .unwrap()
                .contains("unsupported HTTP method")
        );
    }
    for secret in [
        key,
        "password",
        "0123456789abcdef~fedcba9876543210",
        "0123456789abcdef.fedcba9876543210",
    ] {
        assert!(!output.contains(secret), "logs contain a secret: {output}");
    }
}
