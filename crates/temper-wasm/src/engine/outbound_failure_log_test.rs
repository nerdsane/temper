//! An outbound host call that fails leaves a log line saying where it went and
//! what kind of failure it was, and nothing a guest or a response put in it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::field::{Field, Visit};
use tracing_subscriber::prelude::*;

use super::tests::{make_context, make_streams};
use super::{WasmEngine, WasmResourceLimits};
use crate::host_trait::WasmHost;

/// The message every outbound-failure line carries.
const FAILURE_MESSAGE: &str = "WASM host outbound call failed; returning error to guest";

// POSTs to the server's own secrets route, with a query and a body that must
// never reach a log. Offsets: method 2048, url 2056 (57 bytes), body 2200.
const WAT_POST_TO_LOOPBACK: &str = r#"
    (module
      (import "env" "host_http_call" (func $host_http_call
        (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 2048) "POST")
      (data (i32.const 2056) "http://127.0.0.1:8080/paw/setup/secrets?token=QUERYSECRET")
      (data (i32.const 2200) "BODYSECRET")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 2048
        i32.const 4
        i32.const 2056
        i32.const 57
        i32.const 0
        i32.const 0
        i32.const 2200
        i32.const 10
        i32.const 4096
        i32.const 64
        call $host_http_call
        drop
        i32.const 0
      )
    )
"#;

/// Refuses every call the way the production host refuses an internal call
/// that has no capability, with a tail that must not be logged.
struct RefusingHost;

#[async_trait]
impl WasmHost for RefusingHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        Err("internal Temper HTTP call has no authenticated capability issuer: TAILSECRET".into())
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Err("not used".into())
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("test secret not found: {key}"))
    }

    fn log(&self, _level: &str, _message: &str) {}
}

type Events = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

struct Capture(Events);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        fields
            .0
            .insert("level".into(), event.metadata().level().to_string());
        self.0.lock().expect("capture lock").push(fields.0); // ci-ok: test capture
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_outbound_call_logs_target_and_kind_without_secrets() {
    let events: Events = Arc::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Capture(Arc::clone(&events))),
    );

    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_POST_TO_LOOPBACK.as_bytes())
        .unwrap();
    let host: Arc<dyn WasmHost> = Arc::new(RefusingHost);
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            host,
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    assert!(
        result.is_ok(),
        "the guest ignores the -1 and returns: {result:?}"
    );

    let events = events.lock().expect("capture lock").clone(); // ci-ok: test capture
    let failures: Vec<_> = events
        .iter()
        .filter(|fields| fields.get("message").map(String::as_str) == Some(FAILURE_MESSAGE))
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "one line for one failed call, got: {failures:?}"
    );
    let line = failures[0];
    assert_eq!(line.get("level").map(String::as_str), Some("WARN"));
    assert_eq!(
        line.get("host_fn").map(String::as_str),
        Some("host_http_call")
    );
    assert_eq!(line.get("http.method").map(String::as_str), Some("POST"));
    assert_eq!(
        line.get("url.host").map(String::as_str),
        Some("127.0.0.1:8080")
    );
    assert_eq!(
        line.get("url.path").map(String::as_str),
        Some("/paw/setup/secrets")
    );
    assert_eq!(
        line.get("error_kind").map(String::as_str),
        Some("internal Temper HTTP call has no authenticated capability issuer")
    );

    for fields in &events {
        for value in fields.values() {
            for secret in ["QUERYSECRET", "BODYSECRET", "TAILSECRET"] {
                assert!(
                    !value.contains(secret),
                    "{secret} reached a log line: {fields:?}"
                );
            }
        }
    }
}
