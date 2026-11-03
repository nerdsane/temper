//! Bound action helpers for OData write handlers.

mod decide;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::Status;
use temper_runtime::tenant::TenantId;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use temper_authz::SecurityContext;

use super::response::annotate_entity;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::request_context::AgentContext;
use crate::response::{ODataResponse, odata_error, odata_refusal};
use crate::state::redecide::Redecide;
use crate::state::write_record::WriteCaller;
use crate::state::{BoundActionHookContext, DispatchCommand, DispatchError, ServerState};
use decide::{BoundAction, Decision, decide};

/// Notes for the request's handler span: OpenTelemetry attributes and status set
/// on the current tracing span.
struct ActionSpan(tracing::Span);

impl ActionSpan {
    fn set_attribute(&self, attribute: OtelKeyValue) {
        self.0.set_attribute(attribute.key, attribute.value);
    }

    fn set_status(&self, status: Status) {
        self.0.set_status(status);
    }
}

fn idempotency_actor_key(tenant: &TenantId, entity_type: &str, entity_id: &str) -> String {
    format!("{tenant}:{entity_type}:{entity_id}")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_bound_action(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key_str: &str,
    action: &str,
    body_json: serde_json::Value,
    agent_ctx: &AgentContext,
    await_integration: bool,
    idempotency_key: Option<String>,
    security_ctx: &SecurityContext,
) -> axum::response::Response {
    // The bound action's details are attributes of the request's handler span
    // (`POST /odata/{path}`). It is not a span of its own: one request is one
    // server span and one handler span, and only the server span carries the
    // HTTP status (router.rs records it from the response).
    let action_span = ActionSpan(tracing::Span::current());
    for attribute in [
        OtelKeyValue::new("odata.entity_set", set_name.to_string()),
        OtelKeyValue::new("odata.entity_id", key_str.to_string()),
        OtelKeyValue::new("odata.action", action.to_string()),
        OtelKeyValue::new("tenant", tenant.as_str().to_string()),
    ] {
        action_span.set_attribute(attribute);
    }

    if let Some(ref aid) = agent_ctx.agent_id {
        action_span.set_attribute(OtelKeyValue::new("agent.id", aid.clone()));
    }
    if let Some(ref sid) = agent_ctx.session_id {
        action_span.set_attribute(OtelKeyValue::new("session.id", sid.clone()));
    }
    if let Some(ref intent) = agent_ctx.intent {
        action_span.set_attribute(OtelKeyValue::new("intent", intent.clone()));
    }
    for (key, value) in &agent_ctx.observation_metadata {
        action_span.set_attribute(OtelKeyValue::new(
            format!("temper.observation.{key}"),
            value.clone(),
        ));
    }

    let mut dispatch_agent_ctx = agent_ctx.clone();
    dispatch_agent_ctx.security_ctx = Some(security_ctx.clone());

    // Default-deny: reject actions on entity types with no registered spec.
    let is_governed = match state.is_entity_type_governed(tenant, entity_type) {
        Ok(value) => value,
        Err(e) => {
            action_span.set_status(Status::error(e.clone()));
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "RegistryError", &e)
                .into_response();
        }
    };

    if !is_governed {
        action_span.set_status(Status::error("EntityTypeNotGoverned"));
        return odata_error(
            StatusCode::NOT_FOUND,
            "EntityTypeNotGoverned",
            &format!(
                "Entity type '{entity_type}' has no registered spec — actions are denied by default"
            ),
        )
        .into_response();
    }

    let request = BoundAction {
        state,
        tenant,
        entity_type,
        key_str,
        action,
        body_json: &body_json,
        agent_ctx,
        dispatch_agent_ctx: &dispatch_agent_ctx,
        security_ctx,
        span: &action_span,
    };
    let actor_key = idempotency_actor_key(tenant, entity_type, key_str);
    let mut redecide = Redecide::start(state);
    loop {
        let Decision {
            resolved_body,
            decided_on,
        } = match decide(&request, redecide.is_first()).await {
            Ok(decision) => decision,
            Err(undecided) => {
                if redecide.again(undecided.record_changed, "bound_action") {
                    continue;
                }
                return undecided.response;
            }
        };

        // Idempotency cache check
        if let Some(ref idem_key) = idempotency_key
            && let Some(cached) = state.idempotency_cache.get_after_effects_applied(
                &actor_key,
                idem_key,
                &crate::idempotency::RequestFingerprint::new(action, &resolved_body),
            )
        {
            let body = annotate_entity(
                action_answer(&cached.state),
                format!("$metadata#{set_name}/$entity"),
                None,
            );
            action_span.set_attribute(OtelKeyValue::new("idempotency.hit", true));
            action_span.set_status(Status::Ok);
            return ODataResponse {
                status: StatusCode::OK,
                body,
            }
            .into_response();
        }

        redecide.dispatching().await;
        let result = state
            .dispatch_tenant_action_ext_typed_if_current(
                DispatchCommand {
                    tenant,
                    entity_type,
                    entity_id: key_str,
                    action,
                    params: body_json.clone(),
                    agent_ctx: &dispatch_agent_ctx,
                    await_integration,
                    await_reactions: true,
                },
                WriteCaller::of_request(security_ctx),
                decided_on,
            )
            .await;
        // Decided on a record that changed first: an outside caller's write
        // comes back stale, a kernel write's as the actor's refusal. Either is
        // decided again on the record as it is now.
        let decided_on_changed = match &result {
            Ok(answer) => answer.is_record_changed(),
            Err(error) => matches!(error, DispatchError::StaleWrite(_)),
        };
        if redecide.again(decided_on_changed, "bound_action") {
            continue;
        }

        return match result {
            Ok(response) => {
                if response.success {
                    // Dispatch has cached this reply for the key once its effects ran.
                    action_span.set_status(Status::Ok);

                    let mut state_json = action_answer(&response.state);
                    // The action committed; triggers that failed are said beside it.
                    if !response.trigger_failures.is_empty()
                        && let Some(object) = state_json.as_object_mut()
                    {
                        object.insert(
                            "@temper.triggerFailures".to_string(),
                            serde_json::json!(response.trigger_failures),
                        );
                    }
                    if let Some(hook) = state.bound_action_hook.as_ref() {
                        match hook
                            .after_bound_action(BoundActionHookContext {
                                state,
                                tenant,
                                entity_type,
                                entity_id: key_str,
                                action,
                                params: &resolved_body,
                                state_json: &state_json,
                            })
                            .await
                        {
                            Ok(Some(hook_json)) => {
                                if let (Some(dst), Some(src)) =
                                    (state_json.as_object_mut(), hook_json.as_object())
                                {
                                    dst.insert(
                                        "postAction".to_string(),
                                        serde_json::Value::Object(src.clone()),
                                    );
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                action_span.set_status(Status::error(error.clone()));
                                return odata_error(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "PostActionHookFailed",
                                    &error,
                                )
                                .into_response();
                            }
                        }
                    }
                    hydrate_blob_refs_for_tenant(state, tenant, &mut state_json).await;
                    let body =
                        annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
                    ODataResponse {
                        status: StatusCode::OK,
                        body,
                    }
                    .into_response()
                } else if response.store_unavailable {
                    let error = response.error.unwrap_or_default();
                    action_span.set_status(Status::error(error.clone()));
                    crate::write_health::store_unavailable(&error)
                } else {
                    action_span
                        .set_status(Status::error(response.error.clone().unwrap_or_default()));
                    odata_refusal(
                        StatusCode::CONFLICT,
                        "ActionFailed",
                        response.error.as_deref().unwrap_or("Action failed"),
                        response.refusal.as_ref(),
                    )
                    .into_response()
                }
            }
            Err(DispatchError::Ungoverned(entity)) => {
                let reason = format!(
                    "Entity type '{entity}' has no registered spec — actions are denied by default"
                );
                action_span.set_status(Status::error("EntityTypeNotGoverned"));
                odata_error(StatusCode::NOT_FOUND, "EntityTypeNotGoverned", &reason).into_response()
            }
            Err(DispatchError::AuthzDenied(reason)) => {
                action_span.set_status(Status::error(reason.clone()));
                odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response()
            }
            Err(DispatchError::QuotaExceeded(reason)) => {
                action_span.set_status(Status::error(reason.clone()));
                odata_error(StatusCode::PAYLOAD_TOO_LARGE, "StorageCapExceeded", &reason)
                    .into_response()
            }
            Err(DispatchError::StaleWrite(stale)) => {
                action_span.set_status(Status::error(stale.to_string()));
                (StatusCode::CONFLICT, axum::Json(stale.body())).into_response()
            }
            Err(DispatchError::Conflict(reason)) => {
                action_span.set_status(Status::error(reason.clone()));
                odata_error(StatusCode::CONFLICT, "Conflict", &reason).into_response()
            }
            Err(DispatchError::StoreUnavailable(reason)) => {
                action_span.set_status(Status::error(reason.clone()));
                crate::write_health::store_unavailable(&reason)
            }
            // ADR-0048: transient exhaustion → 503 Retry-After so clients and
            // proxies back off instead of paging someone.
            Err(e @ DispatchError::Transient { .. }) => {
                let reason = e.to_string();
                action_span.set_status(Status::error(reason.clone()));
                let mut resp = odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "DispatchTransient",
                    &reason,
                )
                .into_response();
                // Retry-After is per-RFC seconds; 1s is a conservative default
                // until admission control (ADR-0051) can supply a tuned value.
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("1"),
                );
                resp
            }
            // ADR-0051: admission control declined; caller should back off.
            Err(DispatchError::Deferred { retry_after_ms }) => {
                let seconds = retry_after_ms.div_ceil(1000).max(1);
                let reason = format!("dispatch deferred: retry after {retry_after_ms}ms");
                action_span.set_status(Status::error("DispatchDeferred"));
                let mut resp =
                    odata_error(StatusCode::SERVICE_UNAVAILABLE, "DispatchDeferred", &reason)
                        .into_response();
                let value = axum::http::HeaderValue::from_str(&seconds.to_string())
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("1"));
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
                resp
            }
            Err(e) => {
                let reason = e.to_string();
                action_span.set_status(Status::error(reason.clone()));
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DispatchError", &reason)
                    .into_response()
            }
        };
    }
}

// Fresh answers and cached retries show the record as a read does, without its
// recent events. Event params can each hold a whole document; fifty of them
// pushed an endpoint admission's answer past its budget.
fn action_answer(state: &crate::entity_actor::EntityState) -> serde_json::Value {
    let mut record = serde_json::to_value(state).unwrap_or_default();
    if let Some(record) = record.as_object_mut() {
        record.insert("events".to_string(), serde_json::json!([]));
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_actor_key_matches_actor_persistence_id_shape() {
        let tenant = TenantId::new("acme");

        assert_eq!(
            idempotency_actor_key(&tenant, "WorkCycle", "wc-1"),
            "acme:WorkCycle:wc-1"
        );
    }
}
