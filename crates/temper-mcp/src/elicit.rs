//! In-chat governance approvals via MCP elicitation (ADR-0173).
//!
//! When a `temper.*` call is denied by Cedar, the structured denial carries a
//! pending decision id. If the connected MCP client declared the
//! `elicitation` capability at initialize, the server pauses the tool result,
//! sends an `elicitation/create` request so the HUMAN at the client resolves
//! the decision, resolves it against the Temper server with the operator
//! credential (`TEMPER_API_KEY`), and returns the tool result annotated so
//! the model can retry the action. The model never answers the elicitation —
//! the client harness renders it to the human; decline, cancel, or timeout
//! leaves the decision pending and the result unchanged (fail closed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::runtime::RuntimeContext;

/// Default seconds an elicitation waits for the human before the decision is
/// left pending. Override with `TEMPER_MCP_ELICIT_TIMEOUT_SECS`.
const DEFAULT_ELICIT_TIMEOUT_SECS: u64 = 120;

/// Field name and choice values for the elicitation schema.
pub(crate) const CHOICE_APPROVE_NARROW: &str = "approve_narrow";
pub(crate) const CHOICE_APPROVE_BROAD: &str = "approve_broad";
pub(crate) const CHOICE_DENY: &str = "deny";
pub(crate) const CHOICE_LEAVE_PENDING: &str = "leave_pending";

/// In-flight server→client requests keyed by the serialized request id.
#[derive(Clone, Default)]
pub(crate) struct PendingClientRequests {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl PendingClientRequests {
    fn insert(&self, key: String, tx: oneshot::Sender<Value>) {
        self.inner.lock().expect("pending map lock").insert(key, tx);
    }

    fn remove(&self, key: &str) {
        self.inner.lock().expect("pending map lock").remove(key);
    }

    /// Route a client response to the request waiting on its id. Returns
    /// false when no request was waiting (the response is dropped).
    pub(crate) fn resolve(&self, response: Value) -> bool {
        let Some(id) = response.get("id") else {
            return false;
        };
        let key = id.to_string();
        let Some(tx) = self.inner.lock().expect("pending map lock").remove(&key) else {
            return false;
        };
        tx.send(response).is_ok()
    }

    /// Drop every in-flight request so its awaiter fails immediately with
    /// `Closed` instead of waiting out the timeout. Called when the client
    /// stream ends mid-elicitation.
    pub(crate) fn fail_all(&self) {
        self.inner.lock().expect("pending map lock").clear();
    }
}

/// A JSON-RPC message from the client is a *response* to a server→client
/// request when it carries an id and a result/error but no method.
pub(crate) fn is_client_response(message: &Value) -> bool {
    message.get("method").is_none()
        && message.get("id").is_some_and(|id| !id.is_null())
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// Why a server→client request produced no response.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientRequestError {
    /// The outbound channel or the waiting oneshot closed (session ending).
    Closed,
    /// The client did not answer within the timeout.
    Timeout,
}

/// Handle for sending correlated JSON-RPC requests to the connected client.
///
/// Ids are allocated from a server-side counter; responses are matched back
/// through [`PendingClientRequests`] by the stdio reader task.
#[derive(Clone)]
pub(crate) struct ClientRequester {
    outbound: mpsc::UnboundedSender<Value>,
    pending: PendingClientRequests,
    next_id: Arc<AtomicU64>,
}

impl ClientRequester {
    pub(crate) fn new(
        outbound: mpsc::UnboundedSender<Value>,
        pending: PendingClientRequests,
    ) -> Self {
        Self {
            outbound,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Send one request to the client and await its response.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientRequestError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let key = Value::from(id).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(key.clone(), tx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if self.outbound.send(request).is_err() {
            self.pending.remove(&key);
            return Err(ClientRequestError::Closed);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_closed)) => Err(ClientRequestError::Closed),
            Err(_elapsed) => {
                self.pending.remove(&key);
                Err(ClientRequestError::Timeout)
            }
        }
    }
}

/// A Cedar denial observed on a `temper.*` call during one `execute`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeniedDecision {
    /// Tenant the denied call targeted (path segment for resolution).
    pub(crate) tenant: String,
    /// Pending decision id (`PD-...`) created by the server.
    pub(crate) decision_id: String,
    /// Human-readable denial reason from the structured denial body.
    pub(crate) reason: String,
}

/// Extract a resolvable denial from a dispatched `temper.*` result value.
///
/// Only denials that carry a decision id are actionable; a denial without one
/// cannot be resolved and is left for the model to report.
pub(crate) fn denial_from_dispatch_value(tenant: &str, value: &Value) -> Option<DeniedDecision> {
    if value.get("status").and_then(Value::as_str) != Some("authorization_denied") {
        return None;
    }
    let decision_id = value
        .get("decision_id")
        .and_then(Value::as_str)
        .or_else(|| value.get("pending_decision").and_then(Value::as_str))?;
    let reason = value
        .get("reason")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Authorization denied")
        .to_string();
    Some(DeniedDecision {
        tenant: tenant.to_string(),
        decision_id: decision_id.to_string(),
        reason,
    })
}

/// Build the `elicitation/create` params for one pending decision.
///
/// The schema is the MCP restricted flat-object subset: a single enum string
/// field, so every client renders it as a simple choice.
pub(crate) fn elicitation_params(denial: &DeniedDecision, principal: Option<&str>) -> Value {
    let message = format!(
        "Temper blocked an agent action pending human approval.\n\n\
         {reason}\n\n\
         Tenant: {tenant}\n\
         Decision: {id}\n\
         Agent: {agent}\n\n\
         Approving resolves decision {id} with your operator credential and \
         installs a Cedar permit at the chosen scope. \"Leave pending\" keeps \
         the decision open for the Observe UI or `temper decide`.",
        reason = denial.reason,
        tenant = denial.tenant,
        id = denial.decision_id,
        agent = principal.unwrap_or("unknown"),
    );
    json!({
        "message": message,
        "requestedSchema": {
            "type": "object",
            "properties": {
                "decision": {
                    "type": "string",
                    "title": "Resolution",
                    "description": "How to resolve this pending decision",
                    "enum": [
                        CHOICE_APPROVE_NARROW,
                        CHOICE_APPROVE_BROAD,
                        CHOICE_DENY,
                        CHOICE_LEAVE_PENDING,
                    ],
                    "enumNames": [
                        "Approve — this agent, this action, this resource only",
                        "Approve — this agent, all actions on this resource type",
                        "Deny",
                        "Leave pending",
                    ]
                }
            },
            "required": ["decision"]
        }
    })
}

/// How the human resolved (or did not resolve) the elicitation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ElicitChoice {
    ApproveNarrow,
    ApproveBroad,
    Deny,
    LeavePending,
}

/// Parse an elicitation response into a resolution choice.
///
/// Anything other than an explicit `accept` with a recognized choice — a
/// decline, a cancel, an error response, or malformed content — yields
/// `None`, which callers treat as leave-pending (fail closed).
pub(crate) fn parse_elicit_choice(response: &Value) -> Option<ElicitChoice> {
    let result = response.get("result")?;
    if result.get("action").and_then(Value::as_str) != Some("accept") {
        return None;
    }
    match result
        .pointer("/content/decision")
        .and_then(Value::as_str)?
    {
        CHOICE_APPROVE_NARROW => Some(ElicitChoice::ApproveNarrow),
        CHOICE_APPROVE_BROAD => Some(ElicitChoice::ApproveBroad),
        CHOICE_DENY => Some(ElicitChoice::Deny),
        CHOICE_LEAVE_PENDING => Some(ElicitChoice::LeavePending),
        _ => None,
    }
}

/// Narrow approval scope: this agent, this action, this resource, always.
/// Mirrors the `temper decide` CLI's "narrow" choice.
pub(crate) fn narrow_scope() -> Value {
    json!({
        "principal": "this_agent",
        "action": "this_action",
        "resource": "this_resource",
        "duration": "always",
    })
}

/// Broad approval scope: this agent, all actions on the resource type.
/// Mirrors the `temper decide` CLI's "broad" choice.
pub(crate) fn broad_scope() -> Value {
    json!({
        "principal": "this_agent",
        "action": "all_actions_on_type",
        "resource": "any_of_type",
        "duration": "always",
    })
}

/// Whether elicitation approvals are enabled. Any value other than an
/// explicit off switch keeps the default (enabled).
pub(crate) fn elicit_flag_enabled(raw: Option<&str>) -> bool {
    let normalized = raw.map(|value| value.trim().to_ascii_lowercase());
    !matches!(normalized.as_deref(), Some("0" | "false" | "off" | "no"))
}

/// Seconds to wait for the human's elicitation answer.
pub(crate) fn elicit_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_ELICIT_TIMEOUT_SECS)
}

fn elicit_timeout() -> Duration {
    Duration::from_secs(elicit_timeout_secs(
        std::env::var("TEMPER_MCP_ELICIT_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )) // determinism-ok: MCP client runtime config
}

/// Resolve one decision against the Temper server with the MCP's own
/// configured credential (the human operator's key).
async fn resolve_decision(
    ctx: &RuntimeContext,
    denial: &DeniedDecision,
    verb: &str,
    body: Option<Value>,
) -> Result<(), String> {
    let url = format!(
        "{}/api/tenants/{}/decisions/{}/{verb}",
        ctx.base_url, denial.tenant, denial.decision_id
    );
    let mut request = ctx.http.post(&url).header("X-Tenant-Id", &denial.tenant);
    // Post the approval as the approver principal. When a scoped agent
    // credential (api_key) makes the denied call, resolving with a distinct
    // operator credential (approver_key) is required — ARN-389 forbids the
    // denied principal from approving its own decision. Fall back to api_key
    // only when no separate approver is configured (single-principal dev mode).
    let resolve_key = ctx.approver_key.as_ref().or(ctx.api_key.as_ref());
    if let Some(key) = resolve_key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    if let Some(ref payload) = body {
        request = request.json(payload);
    }
    match request.send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            Err(format!("HTTP {status}: {text}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Elicit a human resolution for the first recorded denial and annotate the
/// tool result with the outcome.
///
/// One elicitation per tool call: the first unique denial is put to the
/// human; any further pending decision ids are reported in the annotation so
/// the model can retry and trigger them individually. Decline, cancel,
/// timeout, or an explicit leave-pending returns the result unchanged — the
/// decision is never resolved without an affirmative human answer, and the
/// action is never retried from inside the MCP (the model owns the loop).
pub(crate) async fn apply_denial_elicitation(
    ctx: &RuntimeContext,
    tool_result: Result<String>,
    denials: Vec<DeniedDecision>,
) -> Result<String> {
    if denials.is_empty() || !ctx.elicitation_available() {
        return tool_result;
    }
    let Some(requester) = ctx.requester.clone() else {
        return tool_result;
    };

    let mut unique: Vec<DeniedDecision> = Vec::new();
    for denial in denials {
        if !unique.iter().any(|d| d.decision_id == denial.decision_id) {
            unique.push(denial);
        }
    }
    let denial = unique.remove(0);
    let other_pending: Vec<String> = unique.into_iter().map(|d| d.decision_id).collect();

    let params = elicitation_params(&denial, ctx.agent_id.as_deref());
    let response = match requester
        .request("elicitation/create", params, elicit_timeout())
        .await
    {
        Ok(response) => response,
        Err(ClientRequestError::Timeout) => {
            tracing::warn!(
                decision_id = %denial.decision_id,
                "elicitation timed out; decision left pending"
            );
            return tool_result;
        }
        Err(ClientRequestError::Closed) => {
            tracing::warn!(
                decision_id = %denial.decision_id,
                "elicitation channel closed; decision left pending"
            );
            return tool_result;
        }
    };

    let mut annotation = match parse_elicit_choice(&response) {
        Some(choice @ (ElicitChoice::ApproveNarrow | ElicitChoice::ApproveBroad)) => {
            let (scope, label) = match choice {
                ElicitChoice::ApproveNarrow => (narrow_scope(), "narrow"),
                _ => (broad_scope(), "broad"),
            };
            match resolve_decision(ctx, &denial, "approve", Some(json!({ "scope": scope }))).await {
                Ok(()) => json!({
                    "approval": "granted by human via elicitation",
                    "decision_id": denial.decision_id,
                    "approved_scope": label,
                    "retry": "re-invoke the original action now",
                }),
                Err(error) => {
                    tracing::warn!(
                        decision_id = %denial.decision_id,
                        %error,
                        "elicitation approval failed against the server"
                    );
                    json!({
                        "approval": "resolution failed",
                        "decision_id": denial.decision_id,
                        "error": format!("human approved via elicitation but the approve request failed: {error}"),
                    })
                }
            }
        }
        Some(ElicitChoice::Deny) => match resolve_decision(ctx, &denial, "deny", None).await {
            Ok(()) => json!({
                "approval": "denied by human via elicitation",
                "decision_id": denial.decision_id,
                "note": "do not retry this action",
            }),
            Err(error) => {
                tracing::warn!(
                    decision_id = %denial.decision_id,
                    %error,
                    "elicitation denial failed against the server"
                );
                json!({
                    "approval": "resolution failed",
                    "decision_id": denial.decision_id,
                    "error": format!("human denied via elicitation but the deny request failed: {error}"),
                })
            }
        },
        Some(ElicitChoice::LeavePending) | None => return tool_result,
    };

    if !other_pending.is_empty()
        && let Value::Object(ref mut map) = annotation
    {
        map.insert("other_pending_decisions".to_string(), json!(other_pending));
    }

    annotate_tool_result(tool_result, annotation)
}

/// Merge the elicitation annotation into the tool result text.
///
/// A JSON-object result (the common denial shape) gets the annotation fields
/// merged in; anything else is wrapped so the annotation stays top-level.
/// An error result stays an error, with the annotation appended to its text.
fn annotate_tool_result(tool_result: Result<String>, annotation: Value) -> Result<String> {
    let text = match tool_result {
        Ok(text) => text,
        Err(error) => return Err(anyhow::anyhow!("{error}\nhuman-elicitation: {annotation}")),
    };
    let mut value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    if !value.is_object() {
        value = json!({ "result": value });
    }
    if let Value::Object(fields) = annotation
        && let Value::Object(map) = &mut value
    {
        for (key, field) in fields {
            map.insert(key, field);
        }
    }
    Ok(value.to_string())
}

#[cfg(test)]
#[path = "elicit_test.rs"]
mod tests;
