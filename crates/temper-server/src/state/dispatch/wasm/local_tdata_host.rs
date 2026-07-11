use std::sync::Arc;

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::WasmHost;
use temper_wasm::http_stream::{
    HttpRequestHead, HttpResponseHead, HttpStreamHandles, StreamError, StreamHandle,
};
use tracing::Instrument;

use crate::state::ServerState;

#[path = "local_tdata_host_support.rs"]
mod support;
#[cfg(test)]
use support::is_temper_trust_header;
use support::{
    LocalTDataRequest, callback_registration_header_map, callback_string,
    governance_callback_decision_id, header_map, pending_decision_filter_id,
    security_context_headers, strip_untrusted_temper_headers,
};

const LOCAL_TDATA_RESPONSE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// WASM host wrapper that executes loopback `/tdata` calls in-process.
///
/// This is intentionally a transport optimization only: local calls still run
/// through the same OData handlers as external HTTP traffic.
pub(super) struct LocalTDataWasmHost {
    state: ServerState,
    tenant: TenantId,
    source_entity_type: Option<String>,
    source_entity_id: Option<String>,
    inherited_headers: Vec<(String, String)>,
    delegate: Arc<dyn WasmHost>,
}

impl LocalTDataWasmHost {
    /// Create a local-TData wrapper around an existing host implementation.
    pub(super) fn new(
        state: ServerState,
        tenant: TenantId,
        security_ctx: Option<&SecurityContext>,
        delegate: Arc<dyn WasmHost>,
    ) -> Self {
        Self {
            state,
            tenant,
            source_entity_type: None,
            source_entity_id: None,
            inherited_headers: security_ctx
                .map(security_context_headers)
                .unwrap_or_default(),
            delegate,
        }
    }

    /// Create a host bound to the exact target actor that may mint callback authority.
    pub(super) fn new_for_invocation(
        state: ServerState,
        tenant: TenantId,
        entity_type: &str,
        entity_id: &str,
        security_ctx: Option<&SecurityContext>,
        delegate: Arc<dyn WasmHost>,
    ) -> Self {
        let mut host = Self::new(state, tenant, security_ctx, delegate);
        host.source_entity_type = Some(entity_type.to_string());
        host.source_entity_id = Some(entity_id.to_string());
        host
    }

    fn callback_registration_headers(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        if !method.eq_ignore_ascii_case("POST") {
            return Ok(None);
        }
        if LocalTDataRequest::parse(url, self.state.local_tdata_hosts.as_ref()).is_none() {
            return Ok(None);
        }
        let Some(governance_decision_id) = governance_callback_decision_id(url) else {
            return Ok(None);
        };
        let Some(entity_type) = self.source_entity_type.as_deref() else {
            return Ok(None);
        };
        let Some(entity_id) = self.source_entity_id.as_deref() else {
            return Ok(None);
        };
        let body: serde_json::Value = serde_json::from_str(body)
            .map_err(|error| format!("callback registration body is invalid JSON: {error}"))?;
        let callback_tenant = callback_string(&body, "callback_tenant", "CallbackTenant")?;
        let callback_entity_id = callback_string(&body, "callback_entity_id", "CallbackEntityId")?;
        let approve_action = callback_string(&body, "callback_on_approve", "CallbackOnApprove")?;
        let deny_action = callback_string(&body, "callback_on_deny", "CallbackOnDeny")?;
        if callback_tenant != self.tenant.as_str() || callback_entity_id != entity_id {
            return Err(
                "callback registration target must be the exact invoking tenant/entity".to_string(),
            );
        }
        let capability = self.state.mint_governance_callback_capability(
            &governance_decision_id,
            self.tenant.as_str(),
            entity_type,
            entity_id,
            approve_action,
            deny_action,
        )?;
        let mut signed_headers: Vec<_> = headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("x-temper-callback-capability"))
            .cloned()
            .collect();
        signed_headers.push(("x-temper-callback-capability".to_string(), capability));
        Ok(Some(signed_headers))
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

        if self.state.policy_store().is_some()
            && let Err(error) =
                crate::authz::refresh_policy_snapshot_if_stale(&self.state, self.tenant.as_str())
                    .await
        {
            tracing::error!(tenant = %self.tenant, %error, "local TData policy convergence failed");
            return Err("local TData authorization policy is unavailable".to_string());
        }

        let method_upper = method.to_ascii_uppercase();
        if !matches!(method_upper.as_str(), "GET" | "POST") {
            return Ok(None);
        }
        if method_upper == "GET"
            && let Some(response) = self.local_governance_decision_lookup(&request).await?
        {
            return Ok(Some(response));
        }
        let headers = if governance_callback_decision_id(url).is_some()
            && headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("x-temper-callback-capability"))
        {
            callback_registration_header_map(headers)
        } else {
            header_map(headers, &self.tenant, &self.inherited_headers)
        };
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
                    None,
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

    async fn local_governance_decision_lookup(
        &self,
        request: &LocalTDataRequest,
    ) -> Result<Option<(u16, String)>, String> {
        if request.path != "GovernanceDecisions" {
            return Ok(None);
        }
        let Some(filter) = request.query.get("$filter") else {
            return Ok(None);
        };
        let Some(pending_decision_id) = pending_decision_filter_id(filter) else {
            return Err(
                "GovernanceDecision lookup requires one exact pending_decision_id filter"
                    .to_string(),
            );
        };
        if request.query.get("$top").map(String::as_str) != Some("1") {
            return Err("GovernanceDecision lookup requires $top=1".to_string());
        }
        let Some(store) = self
            .state
            .metadata_store_for_tenant(self.tenant.as_str())
            .await
        else {
            return Err("GovernanceDecision lookup requires durable metadata".to_string());
        };
        let encoded = match store.get_pending_decision(&pending_decision_id).await {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::error!(
                    tenant = %self.tenant,
                    pending_decision_id,
                    %error,
                    "local GovernanceDecision lookup failed"
                );
                return Err("GovernanceDecision lookup is unavailable".to_string());
            }
        };
        let decision = encoded
            .map(|data| {
                serde_json::from_str::<crate::state::PendingDecision>(&data).map_err(|error| {
                    tracing::error!(
                        tenant = %self.tenant,
                        pending_decision_id,
                        %error,
                        "local GovernanceDecision lookup found corrupt durable state"
                    );
                    "GovernanceDecision lookup found invalid durable state".to_string()
                })
            })
            .transpose()?
            .filter(|decision| decision.tenant == self.tenant.as_str());
        let value = decision
            .and_then(|decision| decision.governance_decision_id)
            .map(|id| {
                serde_json::json!({
                    "entity_id": id.clone(),
                    "fields": {"Id": id},
                })
            })
            .into_iter()
            .collect::<Vec<_>>();
        Ok(Some((200, serde_json::json!({"value": value}).to_string())))
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
        let sanitized_headers = strip_untrusted_temper_headers(headers);
        let signed_headers =
            self.callback_registration_headers(method, url, &sanitized_headers, body)?;
        let headers = signed_headers.as_deref().unwrap_or(&sanitized_headers);
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
        let headers = strip_untrusted_temper_headers(headers);
        self.delegate
            .http_call_binary(method, url, &headers, body)
            .await
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let headers = strip_untrusted_temper_headers(headers);
        self.delegate.connect_call(url, &headers, body).await
    }

    async fn http_stream_begin_outbound(
        &self,
        mut request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        request.headers = strip_untrusted_temper_headers(&request.headers);
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

#[cfg(test)]
#[path = "local_tdata_host_test.rs"]
mod tests;

#[cfg(test)]
#[path = "local_tdata_callback_capability_test.rs"]
mod callback_capability_tests;
