//! The action bridge: an HttpEndpoint integration's result turned into an entity action and its HTTP answer.

use super::*;

pub(super) async fn dispatch_action_bridge_result(
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    headers: HeaderMap,
    route_params: std::collections::BTreeMap<String, String>,
    bridge: crate::http_endpoint::HttpActionBridge,
    result: temper_wasm::types::WasmInvocationResult,
) -> Response {
    let tenant_id = authenticated.tenant().clone();
    let callback_params = result.callback_params.clone();
    let refs = bridge_git_receive_pack_refs(&callback_params);
    let sideband = bridge_git_receive_pack_sideband(&callback_params);

    if !result.success {
        return action_bridge_error_response(
            &bridge.response,
            &refs,
            sideband,
            result
                .error
                .as_deref()
                .unwrap_or("adapter returned unsuccessful result"),
        );
    }

    if let Some(short_circuit) = bridge_short_circuit_response(&callback_params) {
        return short_circuit;
    }

    let entity_id = match render_action_bridge_template(&bridge.entity_id_template, &route_params) {
        Ok(id) => id,
        Err(e) => {
            return action_bridge_error_response(&bridge.response, &refs, sideband, &e);
        }
    };
    let action_params = bridge_action_params(&callback_params);

    let mut agent_ctx = crate::request_context::extract_agent_context(&headers);
    agent_ctx.security_ctx = Some(authenticated.security_context().clone());
    agent_ctx.agent_id = Some(authenticated.security_context().principal.id.clone());
    agent_ctx.agent_type = authenticated
        .security_context()
        .principal
        .agent_type
        .clone();
    if agent_ctx.idempotency_key.is_none() {
        agent_ctx.idempotency_key = action_params
            .get("ClientRequestId")
            .or_else(|| action_params.get("client_request_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|key| crate::idempotency::IdempotencyKey::Request(key.to_string()));
    }

    tracing::info!(
        tenant = %tenant_id.as_str(),
        entity_type = %bridge.entity_type,
        entity_id = %entity_id,
        action = %bridge.action,
        response = %bridge.response,
        "HttpEndpoint action bridge dispatching configured action"
    );

    // The record's checks run in the dispatch, on the record as it is. A
    // dispatch decided on a record that changed first is decided again on it
    // (`state::redecide`).
    let mut redecide = crate::state::redecide::Redecide::start(&state);
    let dispatch = loop {
        let dispatch = state
            .dispatch_outside_action(crate::state::DispatchCommand {
                tenant: &tenant_id,
                entity_type: &bridge.entity_type,
                entity_id: &entity_id,
                action: &bridge.action,
                params: action_params.clone(),
                agent_ctx: &agent_ctx,
                await_integration: true,
                // Await reactions like the OData action path
                // (dispatch_bound_action) does, so a composite action
                // dispatched through the bridge — git wire IngestPack,
                // PR merge — has its declared sub-write reactions applied
                // before the protocol response is returned. Without this
                // the reaction is spawned in the background and its
                // sub_writes are silently dropped.
                await_reactions: true,
            })
            .await;
        let stale = matches!(dispatch, Err(crate::state::DispatchError::StaleWrite(_)));
        if !redecide.again(stale, "action_bridge") {
            break dispatch;
        }
    };

    match dispatch {
        Ok(response) if response.success => {
            action_bridge_success_response(&bridge.response, &refs, sideband)
        }
        Ok(response) => action_bridge_refusal_response(
            &bridge.response,
            &refs,
            sideband,
            response
                .error
                .as_deref()
                .unwrap_or("configured action failed"),
            response.refusal.as_ref(),
        ),
        Err(crate::state::DispatchError::StaleWrite(stale))
            if bridge.response != "git-receive-pack" =>
        {
            axum::http::Response::builder()
                .status(axum::http::StatusCode::CONFLICT)
                .header("content-type", "application/json")
                .body(Body::from(stale.body().to_string()))
                .expect("response builder")
        }
        Err(e) => action_bridge_error_response(&bridge.response, &refs, sideband, &e.to_string()),
    }
}

pub(super) fn bridge_action_params(callback_params: &serde_json::Value) -> serde_json::Value {
    if let Some(params) = callback_params
        .get("action_params")
        .or_else(|| callback_params.get("ActionParams"))
    {
        return params.clone();
    }
    // Legacy passthrough adapters return params at the top level; the
    // ADR-0138 control keys must never leak into a dispatched action.
    let mut fallback = callback_params.clone();
    if let Some(map) = fallback.as_object_mut() {
        map.remove("bridge_principal");
        map.remove("bridge_response");
    }
    fallback
}

pub(super) fn git_route_params_for_http_dispatch(
    integration_module: &str,
    request_path: &str,
    mut params: std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    if params.get("owner").is_some_and(|v| !v.is_empty())
        && params.get("repo").is_some_and(|v| !v.is_empty())
    {
        return params;
    }
    if !matches!(
        integration_module,
        "git_refs_advertise" | "git_upload_pack" | "git_receive_pack"
    ) {
        return params;
    }

    let trimmed = request_path.trim_start_matches('/');
    let Some((owner, rest)) = trimmed.split_once('/') else {
        return params;
    };
    let repo = rest
        .strip_suffix(".git/info/refs")
        .or_else(|| rest.strip_suffix(".git/git-upload-pack"))
        .or_else(|| rest.strip_suffix(".git/git-receive-pack"));
    let Some(repo) = repo else {
        return params;
    };
    if owner.is_empty() || repo.is_empty() {
        return params;
    }

    params
        .entry("owner".to_string())
        .or_insert_with(|| owner.to_string());
    params
        .entry("repo".to_string())
        .or_insert_with(|| repo.to_string());
    params
}

/// Adapter short-circuit response for action-bridge routes (ADR-0138).
///
/// Protocol adapters that must refuse a request before any action
/// dispatch — e.g. a 401 challenge on an unauthenticated push — return
/// `bridge_response` with `status`, optional string `headers`, and an
/// optional string `body`. Presence of the key always ends the request:
/// a well-formed value is returned verbatim; a malformed one fails
/// closed as 502 so an adapter's refusal can never decay into a
/// dispatch (which would otherwise run as the system fallback).
pub(super) fn bridge_short_circuit_response(
    callback_params: &serde_json::Value,
) -> Option<Response> {
    let resp = callback_params.get("bridge_response")?;
    // Same passthrough guard as bridge_principal: control keys are
    // honored only in the structured result shape, so a legacy adapter
    // echoing client JSON as top-level params can never hand a caller
    // response control on the kernel origin. Fail closed, not through.
    if callback_params.get("action_params").is_none()
        && callback_params.get("ActionParams").is_none()
    {
        tracing::warn!(
            "bridge_response without explicit action_params fails closed (ADR-0138 structured shape required)"
        );
        return Some(
            axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from(
                    "adapter bridge_response requires the structured result shape",
                ))
                .expect("response builder"),
        );
    }
    Some(match build_bridge_short_circuit(resp) {
        Ok(response) => response,
        Err(reason) => {
            tracing::warn!(%reason, "malformed bridge_response from adapter; failing closed");
            axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from("adapter returned a malformed bridge_response"))
                .expect("response builder")
        }
    })
}

pub(super) fn build_bridge_short_circuit(resp: &serde_json::Value) -> Result<Response, String> {
    let status_code = resp
        .get("status")
        .and_then(|v| v.as_u64())
        .ok_or("bridge_response.status must be a number")?;
    let status = u16::try_from(status_code)
        .ok()
        .and_then(|code| axum::http::StatusCode::from_u16(code).ok())
        .ok_or_else(|| format!("bridge_response.status {status_code} is not a valid status"))?;
    let mut builder = axum::http::Response::builder().status(status);
    if let Some(headers) = resp.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in headers {
            let value = value
                .as_str()
                .ok_or_else(|| format!("bridge_response header {name} must be a string"))?;
            builder = builder.header(name, value);
        }
    }
    let body = resp
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    builder
        .body(Body::from(body))
        .map_err(|e| format!("bridge_response could not be built: {e}"))
}

pub(super) fn bridge_git_receive_pack_refs(callback_params: &serde_json::Value) -> Vec<String> {
    callback_params
        .get("git_receive_pack")
        .or_else(|| callback_params.get("GitReceivePack"))
        .and_then(|v| v.get("refs"))
        .or_else(|| callback_params.get("refs"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn bridge_git_receive_pack_sideband(callback_params: &serde_json::Value) -> bool {
    callback_params
        .get("git_receive_pack")
        .or_else(|| callback_params.get("GitReceivePack"))
        .and_then(|v| v.get("sideband"))
        .or_else(|| callback_params.get("sideband"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub(super) fn render_action_bridge_template(
    template: &str,
    params: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let (prefix, after_open) = rest.split_at(open);
        out.push_str(prefix);
        let after_open = &after_open[1..];
        let Some(close) = after_open.find('}') else {
            return Err(format!(
                "unclosed action bridge placeholder in '{template}'"
            ));
        };
        let key = &after_open[..close];
        let value = params.get(key).ok_or_else(|| {
            format!("action bridge placeholder '{key}' was not captured by the route")
        })?;
        out.push_str(value);
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub(super) fn action_bridge_success_response(
    response_kind: &str,
    refs: &[String],
    sideband: bool,
) -> Response {
    match response_kind {
        "git-receive-pack" => git_receive_pack_response(refs, sideband, None),
        _ => axum::http::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"success":true}"#))
            .expect("response builder"),
    }
}

pub(super) fn action_bridge_error_response(
    response_kind: &str,
    refs: &[String],
    sideband: bool,
    reason: &str,
) -> Response {
    action_bridge_refusal_response(response_kind, refs, sideband, reason, None)
}

/// The bridge's failure answer. A JSON answer carries the action's typed
/// refusal (its `code` and fields) beside `error`, as the OData error does.
pub(super) fn action_bridge_refusal_response(
    response_kind: &str,
    refs: &[String],
    sideband: bool,
    reason: &str,
    refusal: Option<&temper_jit::table::Refusal>,
) -> Response {
    match response_kind {
        "git-receive-pack" => git_receive_pack_response(refs, sideband, Some(reason)),
        _ => {
            let mut body = serde_json::json!({
                "success": false,
                "error": reason,
            });
            if let Some(refusal) = refusal {
                crate::response::merge_refusal(&mut body, refusal);
            }
            axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("response builder")
        }
    }
}

pub(super) fn git_receive_pack_response(
    refs: &[String],
    sideband: bool,
    error: Option<&str>,
) -> Response {
    let reason = error.map(sanitize_git_report_text);
    let mut inner = Vec::new();
    let unpack_line = match reason.as_deref() {
        Some(reason) => format!("unpack {reason}\n"),
        None => "unpack ok\n".to_string(),
    };
    write_pkt_line(&mut inner, unpack_line.as_bytes());
    for refname in refs {
        let line = match reason.as_deref() {
            Some(reason) => format!("ng {refname} {reason}\n"),
            None => format!("ok {refname}\n"),
        };
        write_pkt_line(&mut inner, line.as_bytes());
    }
    write_pkt_flush(&mut inner);

    let body = if sideband {
        sideband_channel_one(inner)
    } else {
        inner
    };

    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/x-git-receive-pack-result")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .expect("response builder")
}

pub(super) fn sideband_channel_one(inner: Vec<u8>) -> Vec<u8> {
    let mut response = Vec::new();
    for chunk in inner.chunks(65_515) {
        let mut payload = Vec::with_capacity(1 + chunk.len());
        payload.push(0x01);
        payload.extend_from_slice(chunk);
        write_pkt_line(&mut response, &payload);
    }
    write_pkt_flush(&mut response);
    response
}

pub(super) fn write_pkt_line(out: &mut Vec<u8>, payload: &[u8]) {
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
}

pub(super) fn write_pkt_flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub(super) fn sanitize_git_report_text(input: &str) -> String {
    let mut out = input
        .chars()
        .map(|c| match c {
            '\r' | '\n' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>();
    if out.len() > 240 {
        out.truncate(240);
    }
    if out.trim().is_empty() {
        "failed".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_jit::table::{GuardCondition, Refusal};

    #[tokio::test]
    async fn a_json_bridge_refusal_carries_the_typed_reason_beside_error() {
        let refusal = Refusal::GuardFailed {
            guard: "holders <= 1".into(),
            conditions: vec![GuardCondition {
                field: "holders".into(),
                op: "<=".into(),
                wants: 1.into(),
                value: 2.into(),
            }],
        };
        let message =
            "Action 'Acquire' blocked from state 'Free': guard 'holders <= 1' failed (holders = 2)";
        let response = action_bridge_refusal_response("json", &[], false, message, Some(&refusal));
        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            body,
            serde_json::json!({
                "success": false,
                "error": message,
                "code": "GuardFailed",
                "guard": "holders <= 1",
                "conditions": [{"field": "holders", "op": "<=", "wants": 1, "value": 2}],
            })
        );
    }
}
