//! End-to-end integration test for ADR-0057 Phase 1 outbound streaming.
//!
//! Spins up a local axum server that echoes POST body bytes back on
//! the response body, then drives a `ProductionWasmHost` through
//! `http_stream_begin_outbound` + streaming read/write/close and
//! verifies the data round-trips cleanly without buffering the
//! full payload in memory.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::{post, put};
use futures_util::StreamExt;
use serde_json::Value;

use temper_wasm::WasmHost;
use temper_wasm::host_trait::ProductionWasmHost;
use temper_wasm::http_stream::{HttpRequestHead, StreamError};
use temper_wasm::types::WasmInvocationContext;

/// Axum handler that streams request body bytes back verbatim.
async fn echo_handler(req: Request) -> impl IntoResponse {
    let (_, body) = req.into_parts();
    let stream = body.into_data_stream();
    let out_stream = stream.map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    Body::from_stream(out_stream)
}

/// Axum handler that reports which headers arrived upstream.
async fn header_echo_handler(req: Request) -> impl IntoResponse {
    let span_hint_present = req.headers().contains_key("x-temper-span-name")
        || req
            .headers()
            .contains_key("x-temper-span-attr-gen_ai.system");
    let regular_header = req
        .headers()
        .get("x-regular")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    format!("span_hint_present={span_hint_present};regular={regular_header}")
}

/// Axum handler that reports auth/context headers plus streamed body bytes.
async fn internal_header_echo_handler(req: Request) -> impl IntoResponse {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, 1024 * 1024).await.unwrap();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    format!(
        "authorization={};tenant={};principal_kind={};principal_id={};workflow_type={};body={}",
        header("authorization"),
        header("x-tenant-id"),
        header("x-temper-principal-kind"),
        header("x-temper-principal-id"),
        header("x-temper-workflow-root-entity-type"),
        String::from_utf8_lossy(&body)
    )
}

/// Bind axum on 127.0.0.1:0 (random port), return the bound addr.
async fn spawn_echo_server() -> String {
    let app = Router::new()
        .route("/echo", post(echo_handler))
        .route("/headers", post(header_echo_handler))
        .route("/internal", put(internal_header_echo_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn outbound_streaming_injects_internal_auth_headers() {
    let base = spawn_echo_server().await;

    let mut secrets = BTreeMap::new();
    secrets.insert("temper_api_url".to_string(), base.clone());
    secrets.insert("temper_api_key".to_string(), "secret123".to_string());

    let host = Arc::new(ProductionWasmHost::new(secrets).with_invocation_context(
        WasmInvocationContext {
            tenant: "default".to_string(),
            entity_type: "Workspace".to_string(),
            entity_id: "ws-1".to_string(),
            trigger_action: "CreateFile".to_string(),
            trigger_params: Value::Null,
            entity_state: Value::Null,
            agent_id: Some("operator".to_string()),
            session_id: None,
            integration_config: BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: Some("CurationQuery".to_string()),
            workflow_root_entity_id: Some("cq-1".to_string()),
            workflow_run_id: Some("CurationQuery:cq-1".to_string()),
            http_request: None,
        },
    ));

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "PUT".into(),
            url: format!("{base}/internal"),
            headers: vec![("content-type".into(), "image/png".into())],
        })
        .await
        .unwrap();

    loop {
        match host
            .http_stream_try_write(handles.request_body, b"raw-image-bytes".to_vec())
            .await
        {
            Ok(_) => break,
            Err(StreamError::WouldBlock) => tokio::task::yield_now().await,
            Err(error) => panic!("unexpected write error: {error:?}"),
        }
    }
    host.http_stream_close(handles.request_body).await.unwrap();

    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 200);

    let mut body = Vec::new();
    loop {
        let chunk = host.http_stream_read(handles.response_body).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).unwrap();

    assert!(body.contains("authorization=Bearer secret123"));
    assert!(body.contains("tenant=default"));
    assert!(body.contains("principal_kind=agent"));
    assert!(body.contains("principal_id=operator"));
    assert!(body.contains("workflow_type=CurationQuery"));
    assert!(body.contains("body=raw-image-bytes"));
}

#[tokio::test]
async fn outbound_streaming_echoes_request_body() {
    let base = spawn_echo_server().await;
    let host = Arc::new(ProductionWasmHost::new(BTreeMap::new()));

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".into(),
            url: format!("{base}/echo"),
            headers: vec![("content-type".into(), "application/octet-stream".into())],
        })
        .await
        .unwrap();

    // Push 4 chunks of known content, then close the request body.
    let chunks: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma", b"delta"];
    for chunk in &chunks {
        host.http_stream_try_write(handles.request_body, chunk.to_vec())
            .await
            .unwrap();
    }
    host.http_stream_close(handles.request_body).await.unwrap();

    // Pull the head, then drain response body until EOF.
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 200);

    let mut echoed = Vec::new();
    loop {
        let chunk = host.http_stream_read(handles.response_body).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        echoed.extend_from_slice(&chunk);
    }

    let expected: Vec<u8> = chunks.into_iter().flat_map(|c| c.to_vec()).collect();
    assert_eq!(echoed, expected, "echoed bytes must match sent");
}

#[tokio::test]
async fn outbound_streaming_strips_temper_span_hint_headers() {
    let base = spawn_echo_server().await;
    let host = Arc::new(ProductionWasmHost::new(BTreeMap::new()));

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".into(),
            url: format!("{base}/headers"),
            headers: vec![
                ("x-regular".into(), "kept".into()),
                ("X-Temper-Span-Name".into(), "tool.llm_call.test".into()),
                (
                    "X-Temper-Span-Attr-gen_ai.system".into(),
                    "test-provider".into(),
                ),
            ],
        })
        .await
        .unwrap();

    host.http_stream_close(handles.request_body).await.unwrap();
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 200);

    let mut body = Vec::new();
    loop {
        let chunk = host.http_stream_read(handles.response_body).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).unwrap();

    assert_eq!(body, "span_hint_present=false;regular=kept");
}

#[tokio::test]
async fn outbound_streaming_1mib_roundtrip() {
    let base = spawn_echo_server().await;
    let host = Arc::new(ProductionWasmHost::new(BTreeMap::new()));

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".into(),
            url: format!("{base}/echo"),
            headers: vec![("content-type".into(), "application/octet-stream".into())],
        })
        .await
        .unwrap();

    // 1 MiB of data in 16 KiB chunks = 64 chunks (exactly channel capacity;
    // exercises the boundary). We interleave writes and reads from the
    // response to keep the request-body channel from filling while the
    // server streams back.
    const CHUNK: usize = 16 * 1024;
    const TOTAL: usize = 1024 * 1024;
    let chunks = TOTAL / CHUNK;
    let host_clone = host.clone();
    let req_handle = handles.request_body;

    let writer = tokio::spawn(async move {
        for i in 0..chunks {
            let mut buf = vec![0u8; CHUNK];
            buf.fill((i & 0xff) as u8);
            loop {
                match host_clone
                    .http_stream_try_write(req_handle, buf.clone())
                    .await
                {
                    Ok(_) => break,
                    Err(temper_wasm::http_stream::StreamError::WouldBlock) => {
                        tokio::task::yield_now().await;
                    }
                    Err(e) => panic!("unexpected write error: {e:?}"),
                }
            }
        }
        host_clone.http_stream_close(req_handle).await.unwrap();
    });

    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 200);

    let mut received: usize = 0;
    loop {
        let chunk = host.http_stream_read(handles.response_body).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        received += chunk.len();
    }
    writer.await.unwrap();
    assert_eq!(received, TOTAL, "received bytes must match sent total");
}
