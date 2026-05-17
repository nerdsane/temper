use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::IntoResponse;
use reqwest::Url;
use temper_wasm::WasmHost;
use tracing::Instrument;

use crate::state::ServerState;

const LOCAL_TDATA_RESPONSE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// WASM host wrapper that executes loopback `/tdata` calls in-process.
///
/// This is intentionally a transport optimization only: local calls still run
/// through the same OData handlers as external HTTP traffic.
pub(super) struct LocalTDataWasmHost {
    state: ServerState,
    delegate: Arc<dyn WasmHost>,
}

impl LocalTDataWasmHost {
    /// Create a local-TData wrapper around an existing host implementation.
    pub(super) fn new(state: ServerState, delegate: Arc<dyn WasmHost>) -> Self {
        Self { state, delegate }
    }

    async fn local_http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Option<(u16, String)>, String> {
        let Some(request) = LocalTDataRequest::parse(url) else {
            return Ok(None);
        };

        let method_upper = method.to_ascii_uppercase();
        if !matches!(method_upper.as_str(), "GET" | "POST") {
            return Ok(None);
        }
        let headers = header_map(headers);
        let path_for_span = request.path.clone();
        let span = tracing::info_span!(
            "wasm.local_tdata_http_call",
            otel.name = "wasm.local_tdata_http_call",
            http.method = %method_upper,
            url.path = %path_for_span,
            local_tdata = true,
        );

        let response = async {
            match method_upper.as_str() {
                "GET" => crate::odata::handle_odata_get(
                    State(self.state.clone()),
                    headers,
                    Path(request.path),
                    Query(request.query),
                )
                .await
                .into_response(),
                "POST" => crate::odata::handle_odata_post(
                    State(self.state.clone()),
                    None,
                    headers,
                    Path(request.path),
                    Query(request.query),
                    Bytes::copy_from_slice(body.as_bytes()),
                )
                .await
                .into_response(),
                _ => unreachable!("local TData method filtered before dispatch"),
            }
        }
        .instrument(span)
        .await;

        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), LOCAL_TDATA_RESPONSE_LIMIT_BYTES)
            .await
            .map_err(|err| format!("failed to read local TData response body: {err}"))?;
        Ok(Some((status, String::from_utf8_lossy(&body).into_owned())))
    }
}

#[async_trait]
impl WasmHost for LocalTDataWasmHost {
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        if let Some(response) = self.local_http_call(method, url, headers, body).await? {
            return Ok(response);
        }
        self.delegate.http_call(method, url, headers, body).await
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.delegate.get_secret(key)
    }

    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        self.delegate
            .http_call_binary(method, url, headers, body)
            .await
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        self.delegate.connect_call(url, headers, body).await
    }

    fn log(&self, level: &str, message: &str) {
        self.delegate.log(level, message);
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<String, String> {
        self.delegate
            .evaluate_spec(ioa_source, current_state, action, params_json)
    }

    fn emit_progress(&self, event_json: &str) -> Result<(), String> {
        self.delegate.emit_progress(event_json)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalTDataRequest {
    path: String,
    query: BTreeMap<String, String>,
}

impl LocalTDataRequest {
    fn parse(url: &str) -> Option<Self> {
        let parsed = Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?;
        if !is_loopback_host(host) {
            return None;
        }

        let raw_path = parsed.path();
        let path = match raw_path {
            "/tdata" | "/tdata/" => String::new(),
            _ => raw_path.strip_prefix("/tdata/")?.to_string(),
        };
        if is_file_value_path(&path) {
            return None;
        }

        let query = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<BTreeMap<_, _>>();

        Some(Self { path, query })
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn is_file_value_path(path: &str) -> bool {
    path.starts_with("Files('") && path.ends_with("')/$value")
}

fn header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        map.insert(name, value);
    }
    map
}

#[cfg(test)]
mod tests {
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
        assert!(LocalTDataRequest::parse("https://api.example.com/tdata/Orders").is_none());
        assert!(LocalTDataRequest::parse("http://127.0.0.1:8787/api/health").is_none());
        assert!(LocalTDataRequest::parse("not a url").is_none());
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
}
