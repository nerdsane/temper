use super::*;

#[tokio::test]
async fn boundary_paths_delegate_to_production_host() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = LocalTDataWasmHost::new(
        test_state(),
        temper_runtime::tenant::TenantId::default(),
        Some(&SecurityContext::system()),
        Arc::new(CountingHost {
            calls: calls.clone(),
            stream_calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let headers = vec![("x-tenant-id".to_string(), "default".to_string())];

    let delegated = [
        (
            "DELETE",
            "http://127.0.0.1:8787/tdata/Orders('order-local-1')",
        ),
        (
            "GET",
            "http://127.0.0.1:8787/tdata/Files('file-local-1')/$value",
        ),
        ("GET", "https://api.example.com/tdata/Orders"),
    ];

    for (method, url) in delegated {
        let (status, body) = host
            .http_call(method, url, &headers, "")
            .await
            .expect("boundary path should delegate");
        assert_eq!(status, 299);
        assert_eq!(body, "delegated");
    }

    assert_eq!(calls.load(Ordering::SeqCst), delegated.len());
}

#[tokio::test]
async fn local_tdata_without_invocation_authority_delegates() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = LocalTDataWasmHost::new(
        test_state(),
        TenantId::default(),
        None,
        Arc::new(CountingHost {
            calls: calls.clone(),
            stream_calls: Arc::new(AtomicUsize::new(0)),
        }),
    );

    let (status, body) = host
        .http_call("GET", "http://127.0.0.1:8787/tdata/Orders", &[], "")
        .await
        .expect("missing typed authority should use authenticated fallthrough");

    assert_eq!(status, 299);
    assert_eq!(body, "delegated");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn outbound_streaming_delegates_to_production_host() {
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let host = LocalTDataWasmHost::new(
        test_state(),
        temper_runtime::tenant::TenantId::default(),
        Some(&SecurityContext::system()),
        Arc::new(CountingHost {
            calls: Arc::new(AtomicUsize::new(0)),
            stream_calls: stream_calls.clone(),
        }),
    );

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            headers: vec![("accept".to_string(), "text/event-stream".to_string())],
        })
        .await
        .expect("local TData wrapper must preserve outbound streaming support");

    assert_eq!(handles.request_body, StreamHandle(11));
    assert_eq!(handles.response_body, StreamHandle(12));
    assert_eq!(
        host.http_stream_try_write(handles.request_body, b"hello".to_vec())
            .await
            .expect("stream writes must delegate"),
        5
    );
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .expect("stream response head must delegate");
    assert_eq!(head.status, 299);
    assert_eq!(
        head.headers,
        vec![("x-test-stream".to_string(), "delegated".to_string())]
    );
    let bounded_chunk = host
        .http_stream_read_bounded(handles.response_body, 1024)
        .await
        .expect("bounded stream reads must delegate");
    assert_eq!(bounded_chunk, b"delegated-bounded-read");
    let direct_chunk = host
        .http_stream_read(handles.response_body)
        .await
        .expect("direct stream reads must delegate");
    assert_eq!(direct_chunk, b"delegated-direct-read");
    host.http_stream_send_response_head(
        handles.response_body,
        HttpResponseHead {
            status: 204,
            headers: Vec::new(),
        },
    )
    .await
    .expect("inbound stream response heads must delegate");
    host.http_stream_close(handles.request_body)
        .await
        .expect("stream close must delegate");

    assert_eq!(stream_calls.load(Ordering::SeqCst), 7);
}
