//! Loopback HTTP interceptor factories for the WASM host chain.
//!
//! These short-circuit blob and `Files(...)/$value` requests that would
//! otherwise loop back through HTTP into this same server process, serving
//! them directly from server state instead.

use std::sync::Arc;

use temper_runtime::tenant::TenantId;
use temper_wasm::{BinaryHttpInterceptorFn, TextHttpInterceptorFn};

use crate::request_context::AgentContext;

pub(super) fn local_blob_binary_interceptor(
    state: crate::state::ServerState,
    tenant: TenantId,
    blob_endpoint: Option<String>,
) -> Option<BinaryHttpInterceptorFn> {
    let endpoint = blob_endpoint?;
    if !crate::blob_store::is_local_internal_blob_endpoint(&endpoint) {
        return None;
    }

    let endpoint = endpoint.trim_end_matches('/').to_string();
    Some(Arc::new(move |method, url, _headers, body| {
        let state = state.clone();
        let tenant = tenant.clone();
        let endpoint = endpoint.clone();
        Box::pin(async move {
            let prefix = format!("{endpoint}/");
            let blob_key = url.strip_prefix(&prefix)?;
            let blob_key = blob_key.to_string();
            crate::runtime_metrics::record_blob_local_fast_path_request(&method);
            tracing::info!(
                method = %method,
                blob_key = %blob_key,
                "handling local blob request without loopback HTTP"
            );

            let result = match method.as_str() {
                "PUT" => state
                    .put_blob_object(&tenant, &blob_key, &body, None)
                    .await
                    .map(|()| (204, Vec::new())),
                "GET" => state
                    .get_blob_with_legacy_fallback(&tenant, &blob_key)
                    .await
                    .map(|maybe| match maybe {
                        Some(bytes) => (200, bytes),
                        None => (404, Vec::new()),
                    }),
                other => Err(format!("unsupported local blob method: {other}")),
            };

            Some(result)
        })
    }))
}

pub(super) fn internal_api_base_url(state: &crate::state::ServerState) -> Option<String> {
    std::env::var("TEMPER_API_URL") // determinism-ok: production host loopback config
        .ok()
        .map(|value| value.trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            state
                .listen_port
                .get()
                .copied()
                .map(|port| format!("http://127.0.0.1:{port}"))
        })
}

fn parse_internal_file_value_request(base_url: &str, url: &str) -> Option<String> {
    let prefix = format!("{}/tdata/Files('", base_url.trim_end_matches('/'));
    let remainder = url.strip_prefix(&prefix)?;
    let file_id = remainder.strip_suffix("')/$value")?;
    Some(file_id.replace("''", "'"))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub(super) fn local_file_value_text_interceptor(
    state: crate::state::ServerState,
    tenant: TenantId,
    agent_ctx: AgentContext,
    temper_api_url: Option<String>,
) -> Option<TextHttpInterceptorFn> {
    let base_url = temper_api_url?.trim_end_matches('/').to_string();
    let is_loopback = base_url.starts_with("http://127.0.0.1:")
        || base_url.starts_with("http://localhost:")
        || base_url.starts_with("http://[::1]:")
        || base_url.starts_with("https://localhost:");
    if !is_loopback {
        return None;
    }

    Some(Arc::new(
        move |method: String, url: String, headers: Vec<(String, String)>, body: String| {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let base_url = base_url.clone();
            Box::pin(async move {
                let file_id = match parse_internal_file_value_request(&base_url, &url) {
                    Some(file_id) => file_id,
                    None => return None,
                };

                tracing::info!(
                    method = %method,
                    file_id = %file_id,
                    "handling internal File $value request without loopback HTTP"
                );

                match method.as_str() {
                    "GET" => {
                        let (status, bytes) = match state
                            .get_file_stream_content(&tenant, &file_id, &agent_ctx)
                            .await
                        {
                            Ok(result) => result,
                            Err(error) => return Some(Err(error)),
                        };
                        if status != 200 {
                            return Some(Ok((status, String::new())));
                        }
                        match String::from_utf8(bytes) {
                            Ok(text) => Some(Ok((200, text))),
                            Err(_) => None,
                        }
                    }
                    "PUT" => {
                        let content_type = header_value(&headers, "content-type")
                            .unwrap_or("application/octet-stream");
                        Some(
                            state
                                .put_file_stream_content(
                                    &tenant,
                                    &file_id,
                                    body.as_bytes(),
                                    content_type,
                                    &agent_ctx,
                                )
                                .await
                                .map(|_| (204, String::new())),
                        )
                    }
                    _ => None,
                }
            })
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_internal_file_value_request_matches_only_value_paths() {
        assert_eq!(
            parse_internal_file_value_request(
                "http://127.0.0.1:3467",
                "http://127.0.0.1:3467/tdata/Files('fl-123')/$value"
            )
            .as_deref(),
            Some("fl-123")
        );
        assert!(
            parse_internal_file_value_request(
                "http://127.0.0.1:3467",
                "http://127.0.0.1:3467/tdata/Files('fl-123')"
            )
            .is_none()
        );
    }
}
