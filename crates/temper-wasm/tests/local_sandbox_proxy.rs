//! Requests to the server's local sandbox carry its token, so they connect to
//! the sandbox directly and never through a configured HTTP proxy. This file
//! holds one test because it sets the proxy variables for its whole process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use temper_wasm::{LocalSandboxAccess, ProductionWasmHost, WasmHost};

const TOKEN: &str = "per-start-token";

/// A loopback server that records the `Authorization` header of each request
/// (`None` when absent) and answers 200.
async fn recording_server() -> (SocketAddr, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let app = Router::new().fallback(move |request: Request| {
        let log = Arc::clone(&log);
        async move {
            let auth = request
                .headers()
                .get(AUTHORIZATION)
                .map(|value| value.to_str().unwrap().to_owned());
            log.lock().unwrap().push(auth);
            "ok"
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, seen)
}

#[tokio::test]
async fn requests_to_the_local_sandbox_do_not_go_through_a_configured_proxy() {
    let (sandbox, sandbox_seen) = recording_server().await;
    let (proxy, proxy_seen) = recording_server().await;
    let proxy_url = format!("http://{proxy}");
    // SAFETY: this test binary runs this one test on a single-threaded
    // runtime, so nothing reads the environment while it changes.
    unsafe {
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(name, &proxy_url);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
    }
    let access = LocalSandboxAccess::new(&format!("http://{sandbox}"), TOKEN.to_string()).unwrap();
    let host = ProductionWasmHost::new(BTreeMap::new()).with_local_sandbox(Some(access));

    let (status, _) = host
        .http_call(
            "POST",
            &format!("http://{sandbox}/v1/processes/run"),
            &[],
            "{}",
        )
        .await
        .unwrap();
    assert_eq!(status, 200);

    assert!(
        proxy_seen.lock().unwrap().is_empty(),
        "the proxy received a sandbox request"
    );
    assert_eq!(
        *sandbox_seen.lock().unwrap(),
        vec![Some(format!("Bearer {TOKEN}"))]
    );
}
