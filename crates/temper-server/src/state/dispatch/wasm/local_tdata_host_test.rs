use super::*;
use axum::http::StatusCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const ORDER_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Local" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Customer" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <Action Name="SubmitOrder" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Local.Order"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.Local.Order"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
"#;

struct FailingHost;

#[async_trait]
impl WasmHost for FailingHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        Err("delegate should not receive local TData calls".to_string())
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
        Err("binary delegate not used".to_string())
    }

    fn log(&self, _level: &str, _message: &str) {}
}

struct CountingHost {
    calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl WasmHost for CountingHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((299, "delegated".to_string()))
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
        Ok((299, b"delegated-binary".to_vec()))
    }

    fn log(&self, _level: &str, _message: &str) {}

    async fn http_stream_begin_outbound(
        &self,
        _request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpStreamHandles {
            request_body: StreamHandle(11),
            response_body: StreamHandle(12),
        })
    }

    async fn http_stream_read(&self, _handle: StreamHandle) -> Result<Vec<u8>, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"delegated-direct-read".to_vec())
    }

    async fn http_stream_read_bounded(
        &self,
        _handle: StreamHandle,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"delegated-bounded-read".to_vec())
    }

    async fn http_stream_try_write(
        &self,
        _handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(chunk.len())
    }

    async fn http_stream_close(&self, _handle: StreamHandle) -> Result<(), StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn http_stream_response_head(
        &self,
        _response_body: StreamHandle,
    ) -> Result<HttpResponseHead, String> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponseHead {
            status: 299,
            headers: vec![("x-test-stream".to_string(), "delegated".to_string())],
        })
    }

    async fn http_stream_send_response_head(
        &self,
        _response_body: StreamHandle,
        _head: HttpResponseHead,
    ) -> Result<(), StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn test_state() -> ServerState {
    let csdl = parse_csdl(ORDER_CSDL_XML).expect("test CSDL should parse");
    let system = ActorSystem::new("local-tdata-wasm-host-test");
    let mut specs = BTreeMap::new();
    specs.insert("Order".to_string(), ORDER_IOA.to_string());
    ServerState::with_specs(system, csdl, ORDER_CSDL_XML.to_string(), specs)
        .expect("test state should build")
}

#[test]
fn parses_loopback_tdata_request() {
    let request = LocalTDataRequest::parse(
        "http://127.0.0.1:8787/tdata/SessionEntries?$filter=SessionId%20eq%20%27s1%27&$top=1",
        &BTreeSet::new(),
    )
    .expect("loopback TData URL should parse");

    assert_eq!(request.path, "SessionEntries");
    assert_eq!(
        request.query.get("$filter").map(String::as_str),
        Some("SessionId eq 's1'")
    );
    assert_eq!(request.query.get("$top").map(String::as_str), Some("1"));
}

#[test]
fn ignores_non_tdata_or_non_loopback_urls() {
    assert!(
        LocalTDataRequest::parse("https://api.example.com/tdata/Orders", &BTreeSet::new())
            .is_none()
    );
    assert!(
        LocalTDataRequest::parse("http://127.0.0.1:8787/api/health", &BTreeSet::new()).is_none()
    );
    assert!(LocalTDataRequest::parse("not a url", &BTreeSet::new()).is_none());
}

#[test]
fn parses_allowlisted_public_tdata_request() {
    let local_hosts = BTreeSet::from(["temper.example".to_string()]);
    let request =
        LocalTDataRequest::parse("https://TEMPER.example/tdata/Orders?$top=1", &local_hosts)
            .expect("allowlisted public TData URL should parse");

    assert_eq!(request.path, "Orders");
    assert_eq!(request.query.get("$top").map(String::as_str), Some("1"));
}

#[tokio::test]
async fn local_tdata_calls_use_odata_handlers() {
    let host = LocalTDataWasmHost::new(test_state(), Arc::new(FailingHost));
    let headers = vec![
        ("x-tenant-id".to_string(), "default".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"id":"order-local-1","Customer":"Ada"}"#,
        )
        .await
        .expect("local create should succeed");
    assert_eq!(status, StatusCode::CREATED.as_u16());
    let created: serde_json::Value = serde_json::from_str(&body).expect("created JSON");
    assert_eq!(created["entity_id"], "order-local-1");

    let (status, body) = host
        .http_call(
            "GET",
            "http://localhost:8787/tdata/Orders('order-local-1')",
            &headers,
            "",
        )
        .await
        .expect("local read should succeed");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["fields"]["Customer"], "Ada");

    let (status, body) = host
        .http_call(
            "POST",
            "http://[::1]:8787/tdata/Orders('order-local-1')/Temper.Local.SubmitOrder",
            &headers,
            "{}",
        )
        .await
        .expect("local action should succeed");
    assert_eq!(status, StatusCode::OK.as_u16());
    let submitted: serde_json::Value = serde_json::from_str(&body).expect("action JSON");
    assert_eq!(submitted["status"], "Submitted");
}

#[tokio::test]
async fn boundary_paths_delegate_to_production_host() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = LocalTDataWasmHost::new(
        test_state(),
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
async fn outbound_streaming_delegates_to_production_host() {
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let host = LocalTDataWasmHost::new(
        test_state(),
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

#[tokio::test]
async fn allowlisted_public_tdata_calls_use_odata_handlers() {
    let mut state = test_state();
    state.local_tdata_hosts = Arc::new(BTreeSet::from(["temper.example".to_string()]));
    let host = LocalTDataWasmHost::new(state, Arc::new(FailingHost));
    let headers = vec![
        ("x-tenant-id".to_string(), "default".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "https://temper.example/tdata/Orders",
            &headers,
            r#"{"id":"order-public-local-1","Customer":"Grace"}"#,
        )
        .await
        .expect("allowlisted public host should dispatch locally");
    assert_eq!(status, StatusCode::CREATED.as_u16());
    let created: serde_json::Value = serde_json::from_str(&body).expect("created JSON");
    assert_eq!(created["entity_id"], "order-public-local-1");

    let (status, body) = host
        .http_call(
            "GET",
            "https://temper.example/tdata/Orders('order-public-local-1')",
            &headers,
            "",
        )
        .await
        .expect("allowlisted public host read should dispatch locally");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["fields"]["Customer"], "Grace");
}
