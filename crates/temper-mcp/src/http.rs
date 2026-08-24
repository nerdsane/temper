//! Stateless Streamable HTTP transport for the local Temper daemon.

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::McpConfig;
use crate::protocol::dispatch_json_value;
use crate::runtime::RuntimeContext;

const HTTP_PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
type ValidationError = (StatusCode, &'static str);

#[derive(Clone)]
struct HttpState {
    config: McpConfig,
    expected_origin_port: u16,
}

/// Build the authenticated, stateless local MCP router.
pub fn http_router(config: McpConfig, expected_origin_port: u16) -> Router {
    Router::new()
        .route("/mcp", post(handle_post))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(HttpState {
            config,
            expected_origin_port,
        })
}

async fn handle_post(
    State(state): State<HttpState>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Response {
    if let Err((status, message)) = validate_headers(&state, &headers) {
        return error(status, message);
    }
    let bytes = match axum::body::to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds budget"),
    };
    let raw: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error_value) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {error_value}"),
            );
        }
    };
    if let Err((status, message)) = validate_envelope(&headers, &raw) {
        return error(status, message);
    }

    let config = state.config.clone();
    let response = match tokio::task::spawn_blocking(move || dispatch_one(config, raw)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error_value)) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &error_value);
        }
        Err(error_value) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &error_value.to_string());
        }
    };

    match response {
        Some(response) => (StatusCode::OK, axum::Json(response)).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn dispatch_one(config: McpConfig, raw: Value) -> Result<Option<Value>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error_value| error_value.to_string())?;
    runtime.block_on(async move {
        let mut context =
            RuntimeContext::from_config(&config).map_err(|error_value| error_value.to_string())?;
        context.allow_host_ops = false;
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": "http-initialize",
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "streamable-http", "version": HTTP_PROTOCOL_VERSION }
            }
        });
        let _ = dispatch_json_value(&mut context, initialize).await;
        let response = dispatch_json_value(&mut context, raw).await;
        context.finalize_trajectory().await;
        Ok(response)
    })
}

fn validate_headers(state: &HttpState, headers: &HeaderMap) -> Result<(), ValidationError> {
    let expected_token = state.config.api_key.as_deref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "local MCP credential is unavailable",
    ))?;
    let expected_authorization = format!("Bearer {expected_token}");
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some(expected_authorization.as_str())
    {
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer credential"));
    }
    if headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        != Some(HTTP_PROTOCOL_VERSION)
    {
        return Err((StatusCode::BAD_REQUEST, "unsupported MCP protocol version"));
    }
    if let Some(origin) = headers.get("origin").and_then(|value| value.to_str().ok()) {
        let allowed = [
            format!("http://127.0.0.1:{}", state.expected_origin_port),
            format!("http://localhost:{}", state.expected_origin_port),
        ];
        if !allowed.iter().any(|candidate| candidate == origin) {
            return Err((StatusCode::FORBIDDEN, "origin is not allowed"));
        }
    }
    Ok(())
}

fn validate_envelope(headers: &HeaderMap, raw: &Value) -> Result<(), ValidationError> {
    let method = raw
        .get("method")
        .and_then(Value::as_str)
        .ok_or((StatusCode::BAD_REQUEST, "JSON-RPC method is required"))?;
    if headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        != Some(method)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Mcp-Method does not match the request",
        ));
    }
    let body_name = raw.pointer("/params/name").and_then(Value::as_str);
    let header_name = headers
        .get("mcp-name")
        .and_then(|value| value.to_str().ok());
    if method == "tools/call" && (body_name.is_none() || body_name != header_name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Mcp-Name does not match the tool call",
        ));
    }
    Ok(())
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Method, Request};
    use tower::ServiceExt as _;

    fn state() -> HttpState {
        HttpState {
            config: McpConfig {
                temper_port: Some(3000),
                temper_url: None,
                agent_id: None,
                agent_type: None,
                session_id: None,
                api_key: Some("secret".to_string()),
            },
            expected_origin_port: 3000,
        }
    }

    #[test]
    fn headers_require_auth_protocol_and_allowed_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(HTTP_PROTOCOL_VERSION),
        );
        headers.insert("origin", HeaderValue::from_static("http://127.0.0.1:3000"));
        assert!(validate_headers(&state(), &headers).is_ok());
        headers.insert(
            "origin",
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(validate_headers(&state(), &headers).is_err());
    }

    #[test]
    fn envelope_headers_must_match_body() {
        let raw = json!({"method":"tools/call", "params":{"name":"execute"}});
        let mut headers = HeaderMap::new();
        headers.insert("mcp-method", HeaderValue::from_static("tools/call"));
        headers.insert("mcp-name", HeaderValue::from_static("execute"));
        assert!(validate_envelope(&headers, &raw).is_ok());
        headers.insert("mcp-name", HeaderValue::from_static("other"));
        assert!(validate_envelope(&headers, &raw).is_err());
    }

    #[tokio::test]
    async fn router_rejects_session_methods_and_unauthenticated_posts() {
        let router = http_router(state().config, 3000);
        for method in [Method::GET, Method::DELETE] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/mcp")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
        let response = router
            .oneshot(Request::post("/mcp").body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
