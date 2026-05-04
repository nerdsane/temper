use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use temper_wasm::http_stream::{
    HttpRequestHead, HttpResponseHead, HttpStreamHandles, StreamError, StreamHandle,
};
use temper_wasm::{
    AuthorizedWasmHost, WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate, WasmHost,
};

struct DenyAllGate;

impl WasmAuthzGate for DenyAllGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }

    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }
}

struct AllowAllGate;

impl WasmAuthzGate for AllowAllGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }

    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

#[derive(Default)]
struct RecordingStreamHost {
    begin_requests: Mutex<Vec<HttpRequestHead>>,
    read_calls: Mutex<Vec<StreamHandle>>,
    try_write_calls: Mutex<Vec<(StreamHandle, Vec<u8>)>>,
    close_calls: Mutex<Vec<StreamHandle>>,
    response_head_calls: Mutex<Vec<StreamHandle>>,
}

#[async_trait]
impl WasmHost for RecordingStreamHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        Ok((200, String::new()))
    }

    fn get_secret(&self, _key: &str) -> Result<String, String> {
        Ok(String::new())
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Ok((200, Vec::new()))
    }

    fn log(&self, _level: &str, _message: &str) {}

    async fn http_stream_begin_outbound(
        &self,
        request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        self.begin_requests.lock().unwrap().push(request);
        Ok(HttpStreamHandles {
            request_body: StreamHandle(11),
            response_body: StreamHandle(12),
        })
    }

    async fn http_stream_read(&self, handle: StreamHandle) -> Result<Vec<u8>, StreamError> {
        self.read_calls.lock().unwrap().push(handle);
        Ok(Vec::new())
    }

    async fn http_stream_read_bounded(
        &self,
        handle: StreamHandle,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        self.read_calls.lock().unwrap().push(handle);
        Ok(b"chunk".to_vec())
    }

    async fn http_stream_try_write(
        &self,
        handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        self.try_write_calls
            .lock()
            .unwrap()
            .push((handle, chunk.clone()));
        Ok(chunk.len())
    }

    async fn http_stream_close(&self, handle: StreamHandle) -> Result<(), StreamError> {
        self.close_calls.lock().unwrap().push(handle);
        Ok(())
    }

    async fn http_stream_response_head(
        &self,
        response_body: StreamHandle,
    ) -> Result<HttpResponseHead, String> {
        self.response_head_calls.lock().unwrap().push(response_body);
        Ok(HttpResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        })
    }
}

fn test_ctx() -> WasmAuthzContext {
    WasmAuthzContext {
        tenant: "test-tenant".into(),
        module_name: "provider_caller".into(),
        agent_id: Some("agent-1".into()),
        session_id: None,
        entity_type: "SessionTurn".into(),
        trigger_action: "PreparedContextReady".into(),
    }
}

#[tokio::test]
async fn deny_gate_blocks_http_stream_begin_outbound() {
    let inner = Arc::new(RecordingStreamHost::default());
    let gate = Arc::new(DenyAllGate);
    let host = AuthorizedWasmHost::new(inner.clone(), gate, test_ctx());

    let result = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".into(),
            url: "https://api.openai.com/v1/responses".into(),
            headers: vec![],
        })
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("authorization denied"));
    assert!(inner.begin_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn allow_gate_delegates_http_stream_methods() {
    let inner = Arc::new(RecordingStreamHost::default());
    let gate = Arc::new(AllowAllGate);
    let host = AuthorizedWasmHost::new(inner.clone(), gate, test_ctx());

    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "POST".into(),
            url: "https://api.openai.com/v1/responses".into(),
            headers: vec![("accept".into(), "text/event-stream".into())],
        })
        .await
        .expect("stream begin should delegate");

    assert_eq!(handles.request_body, StreamHandle(11));
    assert_eq!(handles.response_body, StreamHandle(12));
    assert_eq!(
        inner.begin_requests.lock().unwrap()[0].url,
        "https://api.openai.com/v1/responses"
    );

    let written = host
        .http_stream_try_write(handles.request_body, b"body".to_vec())
        .await
        .expect("stream write should delegate");
    assert_eq!(written, 4);

    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .expect("response head should delegate");
    assert_eq!(head.status, 200);

    let chunk = host
        .http_stream_read_bounded(handles.response_body, 16)
        .await
        .expect("stream read should delegate");
    assert_eq!(chunk, b"chunk");

    host.http_stream_close(handles.response_body)
        .await
        .expect("stream close should delegate");

    assert_eq!(
        inner.try_write_calls.lock().unwrap()[0],
        (StreamHandle(11), b"body".to_vec())
    );
    assert_eq!(
        inner.response_head_calls.lock().unwrap()[0],
        StreamHandle(12)
    );
    assert_eq!(inner.read_calls.lock().unwrap()[0], StreamHandle(12));
    assert_eq!(inner.close_calls.lock().unwrap()[0], StreamHandle(12));
}
