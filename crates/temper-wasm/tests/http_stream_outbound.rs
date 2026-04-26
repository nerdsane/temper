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
use axum::body::Body;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::post;
use futures_util::StreamExt;

use temper_wasm::WasmHost;
use temper_wasm::host_trait::ProductionWasmHost;
use temper_wasm::http_stream::HttpRequestHead;

/// Axum handler that streams request body bytes back verbatim.
async fn echo_handler(req: Request) -> impl IntoResponse {
    let (_, body) = req.into_parts();
    let stream = body.into_data_stream();
    let out_stream = stream.map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    Body::from_stream(out_stream)
}

/// Bind axum on 127.0.0.1:0 (random port), return the bound addr.
async fn spawn_echo_server() -> String {
    let app = Router::new().route("/echo", post(echo_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
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
