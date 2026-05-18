use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::IntoResponse;
use reqwest::Url;
use temper_runtime::tenant::TenantId;
use temper_wasm::WasmHost;
use temper_wasm::http_stream::{
    HttpRequestHead, HttpResponseHead, HttpStreamHandles, StreamError, StreamHandle,
};
use tracing::Instrument;

use crate::state::ServerState;

const LOCAL_TDATA_RESPONSE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// WASM host wrapper that executes loopback `/tdata` calls in-process.
///
/// This is intentionally a transport optimization only: local calls still run
/// through the same OData handlers as external HTTP traffic.
pub(super) struct LocalTDataWasmHost {
    state: ServerState,
    tenant: TenantId,
    delegate: Arc<dyn WasmHost>,
}

impl LocalTDataWasmHost {
    /// Create a local-TData wrapper around an existing host implementation.
    pub(super) fn new(state: ServerState, tenant: TenantId, delegate: Arc<dyn WasmHost>) -> Self {
        Self {
            state,
            tenant,
            delegate,
        }
    }

    async fn local_http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Option<(u16, String)>, String> {
        let Some(request) = LocalTDataRequest::parse(url, self.state.local_tdata_hosts.as_ref())
        else {
            return Ok(None);
        };

        let method_upper = method.to_ascii_uppercase();
        if !matches!(method_upper.as_str(), "GET" | "POST") {
            return Ok(None);
        }
        let headers = header_map(headers, &self.tenant);
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

    async fn http_stream_begin_outbound(
        &self,
        request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        self.delegate.http_stream_begin_outbound(request).await
    }

    async fn http_stream_read(&self, handle: StreamHandle) -> Result<Vec<u8>, StreamError> {
        self.delegate.http_stream_read(handle).await
    }

    async fn http_stream_read_bounded(
        &self,
        handle: StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        self.delegate
            .http_stream_read_bounded(handle, max_bytes)
            .await
    }

    async fn http_stream_try_write(
        &self,
        handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        self.delegate.http_stream_try_write(handle, chunk).await
    }

    async fn http_stream_close(&self, handle: StreamHandle) -> Result<(), StreamError> {
        self.delegate.http_stream_close(handle).await
    }

    async fn http_stream_response_head(
        &self,
        response_body: StreamHandle,
    ) -> Result<HttpResponseHead, String> {
        self.delegate.http_stream_response_head(response_body).await
    }

    async fn http_stream_send_response_head(
        &self,
        response_body: StreamHandle,
        head: HttpResponseHead,
    ) -> Result<(), StreamError> {
        self.delegate
            .http_stream_send_response_head(response_body, head)
            .await
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

    fn emit_wide_event(&self, event_json: &str) -> Result<(), String> {
        self.delegate.emit_wide_event(event_json)
    }

    fn log_structured(&self, log_json: &str) -> Result<(), String> {
        self.delegate.log_structured(log_json)
    }

    fn emit_metric(&self, metric_json: &str) -> Result<(), String> {
        self.delegate.emit_metric(metric_json)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalTDataRequest {
    path: String,
    query: BTreeMap<String, String>,
}

impl LocalTDataRequest {
    fn parse(url: &str, local_tdata_hosts: &BTreeSet<String>) -> Option<Self> {
        let parsed = Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?;
        if !is_local_tdata_host(host, local_tdata_hosts) {
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

fn is_local_tdata_host(host: &str, local_tdata_hosts: &BTreeSet<String>) -> bool {
    if is_loopback_host(host) {
        return true;
    }
    local_tdata_hosts.contains(&host.to_ascii_lowercase())
}

fn is_file_value_path(path: &str) -> bool {
    path.starts_with("Files('") && path.ends_with("')/$value")
}

fn header_map(headers: &[(String, String)], tenant: &TenantId) -> HeaderMap {
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
    if !map.contains_key("x-tenant-id") {
        let value =
            HeaderValue::from_str(tenant.as_str()).expect("TenantId is a valid HTTP header value");
        map.insert(HeaderName::from_static("x-tenant-id"), value);
    }
    map
}

#[cfg(test)]
#[path = "local_tdata_host_test.rs"]
mod tests;
