//! Requests to the server's local sandbox carry the sandbox token, which the
//! host adds; requests anywhere else do not. Drives the real
//! `ProductionWasmHost` (reqwest) against loopback servers that record the
//! `Authorization` header each request arrives with.

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

fn host_with_sandbox(sandbox: SocketAddr) -> ProductionWasmHost {
    let access = LocalSandboxAccess::new(&format!("http://{sandbox}"), TOKEN.to_string()).unwrap();
    ProductionWasmHost::new(BTreeMap::new()).with_local_sandbox(Some(access))
}

#[tokio::test]
async fn requests_to_the_local_sandbox_carry_its_token() {
    let (sandbox, seen) = recording_server().await;
    let host = host_with_sandbox(sandbox);

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
    let (status, _) = host
        .http_call(
            "GET",
            &format!("http://{sandbox}/v1/fs/file?path=a.txt"),
            &[],
            "",
        )
        .await
        .unwrap();
    assert_eq!(status, 200);

    let expected = Some(format!("Bearer {TOKEN}"));
    assert_eq!(*seen.lock().unwrap(), vec![expected.clone(), expected]);
}

#[tokio::test]
async fn requests_to_other_origins_do_not_carry_the_sandbox_token() {
    let (sandbox, sandbox_seen) = recording_server().await;
    let (other, other_seen) = recording_server().await;
    let host = host_with_sandbox(sandbox);

    // Same host, another port: a different origin.
    let (status, _) = host
        .http_call(
            "POST",
            &format!("http://{other}/v1/processes/run"),
            &[],
            "{}",
        )
        .await
        .unwrap();
    assert_eq!(status, 200);
    // Same port under another name: also a different origin.
    let (status, _) = host
        .http_call(
            "GET",
            &format!("http://localhost:{}/health", sandbox.port()),
            &[],
            "",
        )
        .await
        .unwrap();
    assert_eq!(status, 200);

    assert_eq!(*other_seen.lock().unwrap(), vec![None]);
    assert_eq!(*sandbox_seen.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn a_host_without_a_local_sandbox_adds_no_token() {
    let (server, seen) = recording_server().await;
    let host = ProductionWasmHost::new(BTreeMap::new());

    let (status, _) = host
        .http_call("GET", &format!("http://{server}/health"), &[], "")
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(*seen.lock().unwrap(), vec![None]);
}

#[test]
fn the_sandbox_token_is_not_printed_in_debug_output() {
    let access = LocalSandboxAccess::new("http://127.0.0.1:3010", TOKEN.to_string()).unwrap();
    let printed = format!("{access:?}");
    assert!(printed.contains("http://127.0.0.1:3010"), "{printed}");
    assert!(!printed.contains(TOKEN), "{printed}");
}
