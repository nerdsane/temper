//! Spec-declared checks performed as the authenticated caller before external I/O.
use crate::{ServerState, response::odata_error};
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use temper_authz::AuthenticatedRequestContext;

/// An ordinary OData bound action, using path captures as typed string inputs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionAction {
    /// Key under which the admitted response is given to the transport.
    pub name: String,
    /// Entity set from CSDL.
    pub entity_set: String,
    /// Entity ID: literal or an entire `{capture}` placeholder.
    pub entity_id: String,
    /// Qualified action name from CSDL.
    pub action: String,
    /// String parameters: literals or entire `{capture}` placeholders.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

pub(super) fn resolve(value: &str, captures: &BTreeMap<String, String>) -> Result<String, String> {
    if let Some(key) = value.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        let value = captures
            .get(key)
            .ok_or_else(|| format!("missing route capture {key}"))?;
        // Decode once, as data, never as a path or OData expression.
        let value = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| "route capture is not UTF-8".to_string())?;
        Ok(value.into_owned())
    } else if value.contains(['{', '}']) {
        Err("route placeholders must occupy the entire value".into())
    } else {
        Ok(value.to_owned())
    }
}

/// The most of one admission action's answer held for the module. Its reason is memory: it is held per request
/// before the module runs. An answer is the record as a read shows it (bindings/mod.rs), so only a record whose own
/// fields approach this reaches it.
const ANSWER_BUDGET_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn admit(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    endpoint_id: &str,
    captures: &BTreeMap<String, String>,
    actions: Vec<AdmissionAction>,
) -> Result<BTreeMap<String, Value>, Response<Body>> {
    // A refusal here answers before the module runs, so this log is the only place that says why.
    let invalid = |message: &str| {
        tracing::warn!(
            endpoint_id,
            reason = message,
            "endpoint admission refused the request"
        );
        odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InvalidEndpoint",
            message,
        )
        .into_response()
    };
    let mut prepared = Vec::new();
    let mut names = BTreeSet::new();
    for gate in actions {
        if !names.insert(gate.name.clone()) {
            return Err(invalid("duplicate admission name"));
        }
        // Set/action names are schema identifiers, never route-derived syntax.
        if ![&gate.entity_set, &gate.action].iter().all(|s| {
            s.split('.').all(|part| {
                part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        }) {
            return Err(invalid("invalid CSDL identifier in admission"));
        }
        let id = match resolve(&gate.entity_id, captures) {
            Ok(v) => v,
            Err(e) => return Err(invalid(&e)),
        };
        let mut params = serde_json::Map::new();
        for (key, value) in gate.params {
            match resolve(&value, captures) {
                Ok(value) => {
                    params.insert(key, json!(value));
                }
                Err(error) => return Err(invalid(&error)),
            }
        }
        let path = format!(
            "{}('{}')/{}",
            gate.entity_set,
            id.replace('\'', "''"),
            gate.action
        );
        if !matches!(temper_odata::parse_path(&path), Ok(temper_odata::ODataPath::BoundAction { parent, .. })
            if matches!(*parent, temper_odata::ODataPath::Entity(..)))
        {
            return Err(invalid("invalid OData admission path"));
        }
        let entity_type =
            crate::odata::resolve_entity_type(state, authenticated.tenant(), &gate.entity_set);
        if entity_type
            .as_ref()
            .is_some_and(|ty| state.is_pg_actor_backed(authenticated.tenant(), ty))
        {
            // PG dispatch acknowledges enqueueing, not successful IOA execution.
            // Reject the whole declaration before ANY action can be queued.
            return Err(requires_completion());
        }
        prepared.push((
            gate.name,
            path,
            Bytes::from(Value::Object(params).to_string()),
        ));
    }
    let mut admitted = BTreeMap::new();
    for (name, path, params) in prepared {
        // Do not inherit a user idempotency key: admission is evaluated on EVERY
        // request, even when the remote application receives a repeated key.
        // Boxed: the OData handler's future is among the largest in the
        // kernel; inlined, every future above this route call would carry it.
        let response = Box::pin(crate::odata::handle_odata_post(
            State(state.clone()),
            Some(Extension(authenticated.clone())),
            HeaderMap::new(),
            Path(path.clone()),
            Query(BTreeMap::new()),
            params,
        ))
        .await
        .into_response();
        if response.status() == StatusCode::ACCEPTED {
            return Err(requires_completion());
        }
        if !response.status().is_success() {
            return Err(log_refusal_response(endpoint_id, &name, &path, response).await);
        }
        let bytes = match to_bytes(response.into_body(), ANSWER_BUDGET_BYTES).await {
            Ok(v) => v,
            Err(_) => {
                return Err(invalid(&format!(
                    "the answer of {path} exceeds {ANSWER_BUDGET_BYTES} bytes"
                )));
            }
        };
        let value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return Err(invalid("admission returned invalid JSON")),
        };
        admitted.insert(name, value);
    }
    Ok(admitted)
}

fn requires_completion() -> Response {
    odata_error(StatusCode::SERVICE_UNAVAILABLE, "AdmissionRequiresCompletion",
        "endpoint admission requires completed OData actions; queued PostgreSQL actions are not supported")
        .into_response()
}

async fn log_refusal_response(
    endpoint_id: &str,
    name: &str,
    path: &str,
    response: Response,
) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();
    // OData refusals can contain stored field values. Keep their original body,
    // even when it is larger than a successful admission answer's budget.
    let body = match to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(endpoint_id, admission = %name, %path, %status, %error,
                "endpoint admission refusal body could not be read");
            // Keep the status, headers and body failure instead of turning an
            // unreadable refusal into a successful read of an empty body.
            let body =
                Body::from_stream(futures_util::stream::once(
                    async move { Err::<Bytes, _>(error) },
                ));
            return Response::from_parts(parts, body);
        }
    };
    let reason = String::from_utf8_lossy(&body);
    if status.is_server_error() {
        tracing::warn!(endpoint_id, admission = %name, %path, %status, %reason, "endpoint admission action failed");
    } else {
        tracing::info!(endpoint_id, admission = %name, %path, %status, %reason, "endpoint admission action refused");
    }
    Response::from_parts(parts, Body::from(body))
}

#[cfg(test)]
mod tests;
