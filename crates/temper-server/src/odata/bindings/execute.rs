use super::*;

pub(super) struct BoundActionExecution<'a> {
    pub(super) state: &'a ServerState,
    pub(super) tenant: &'a TenantId,
    pub(super) set_name: &'a str,
    pub(super) entity_type: &'a str,
    pub(super) key_str: &'a str,
    pub(super) action: &'a str,
    pub(super) body_json: serde_json::Value,
    pub(super) agent_ctx: &'a AgentContext,
    pub(super) headers: &'a HeaderMap,
    pub(super) await_integration: bool,
    pub(super) idempotency_key: Option<String>,
    pub(super) resolved_identity: Option<&'a ResolvedIdentity>,
    pub(super) generation_lease: Option<&'a TenantGenerationLease>,
    pub(super) security_ctx: SecurityContext,
    pub(super) dispatch_agent_ctx: AgentContext,
    pub(super) operation_fingerprint: String,
    pub(super) request_fingerprint: String,
    pub(super) actor_key: String,
}

pub(super) async fn execute_bound_action<S: Span + Send>(
    execution: BoundActionExecution<'_>,
    mut http_span: S,
) -> axum::response::Response {
    let BoundActionExecution {
        state,
        tenant,
        set_name,
        entity_type,
        key_str,
        action,
        body_json,
        agent_ctx,
        headers,
        await_integration,
        idempotency_key,
        resolved_identity,
        generation_lease,
        security_ctx,
        mut dispatch_agent_ctx,
        operation_fingerprint,
        request_fingerprint,
        actor_key,
    } = execution;

    let authz_snapshot = match state
        .load_authz_resource_snapshot(tenant, entity_type, key_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            http_span.set_status(Status::error(e.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
            let end_time: std::time::SystemTime = sim_now().into();
            http_span.end_with_timestamp(end_time);
            let code = if e.contains("registry lock poisoned") {
                "RegistryError"
            } else {
                "ReadError"
            };
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, code, &e).into_response();
        }
    };
    let current_state = authz_snapshot.current_state;
    let resource_attrs = authz_snapshot.resource_attrs;

    if let Err(resp) = enforce_commons_account_verified_for_action(
        state,
        tenant,
        entity_type,
        &current_state.state.fields,
        &body_json,
    )
    .await
    {
        http_span.set_status(Status::error("AccountVerificationRequired"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 403i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return *resp;
    }

    if let Err(resp) = enforce_commons_write_rate_limit(
        state,
        tenant,
        entity_type,
        owner_id_from_action(&current_state.state.fields, &body_json),
        headers,
        agent_ctx,
        resolved_identity,
    )
    .await
    {
        http_span.set_status(Status::error("RateLimitExceeded"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 429i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return resp;
    }

    let authz_result = state.authorize_with_context(
        &security_ctx,
        action,
        entity_type,
        &resource_attrs,
        tenant.as_str(),
    );
    if let Err(denial) = authz_result {
        let reason = denial.to_string();
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant: tenant.as_str(),
                security_ctx: &security_ctx,
                agent_id_override: agent_ctx.agent_id.as_deref(),
                action,
                resource_type: entity_type,
                resource_id: key_str,
                resource_attrs: serde_json::to_value(&resource_attrs).unwrap_or_default(),
                reason: &reason,
                module_name: None,
                from_status: Some(current_state.state.status.clone()),
            },
        )
        .await;

        http_span.set_status(Status::error(reason.clone()));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        let reason_with_id = format!("{reason} (decision: {})", pd.id);
        return odata_error(
            StatusCode::FORBIDDEN,
            "AuthorizationDenied",
            &reason_with_id,
        )
        .into_response();
    }

    let current_fields = current_state.state.fields.clone();
    if let Err(resp) = run_write_prechecks(
        state,
        tenant,
        entity_type,
        key_str,
        action,
        "bound_action",
        &current_fields,
    )
    .await
    {
        http_span.set_status(Status::error("ConstraintViolation"));
        http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
        let end_time: std::time::SystemTime = sim_now().into();
        http_span.end_with_timestamp(end_time);
        return resp;
    }

    let mut reservation_guard = None;
    if let Some(ref idem_key) = idempotency_key {
        let (durable_prefix, _operation_prefix, durable_key) = bound_action_durable_idempotency_key(
            idem_key,
            &operation_fingerprint,
            &request_fingerprint,
        );
        let durable_conflict =
            current_state
                .state
                .processed_idempotency_keys
                .keys()
                .any(|stored| {
                    stored == idem_key
                        || (stored.starts_with(&durable_prefix) && stored != &durable_key)
                });
        if durable_conflict {
            return odata_error(
                StatusCode::CONFLICT,
                "IdempotencyConflict",
                "Idempotency-Key was already durably used by a different or unproved action request",
            )
            .into_response();
        }

        match state.idempotency_cache.claim_bound_action(
            &actor_key,
            idem_key,
            &request_fingerprint,
            &body_json,
        ) {
            BoundActionClaim::Conflict => {
                return odata_error(
                    StatusCode::CONFLICT,
                    "IdempotencyConflict",
                    "Idempotency-Key was already used by a different action request or principal",
                )
                .into_response();
            }
            BoundActionClaim::AtCapacity => {
                return odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "IdempotencyCapacityExceeded",
                    "This entity has too many idempotent actions in progress; retry later",
                )
                .into_response();
            }
            BoundActionClaim::Pending => {
                return odata_error(
                    StatusCode::CONFLICT,
                    "IdempotencyInProgress",
                    "An identical request with this Idempotency-Key is still in progress",
                )
                .into_response();
            }
            BoundActionClaim::Match {
                response: cached,
                params: original_params,
                hook_completed,
                hook_output,
            } => {
                let mut state_json = serde_json::to_value(&cached.state).unwrap_or_default();
                if hook_completed {
                    merge_bound_action_hook_output(&mut state_json, hook_output.as_ref());
                } else {
                    if let Err(error) = run_or_recover_bound_action_hook(
                        state,
                        tenant,
                        entity_type,
                        key_str,
                        action,
                        &original_params,
                        &mut state_json,
                        generation_lease,
                        &actor_key,
                        idem_key,
                        &operation_fingerprint,
                        &request_fingerprint,
                    )
                    .await
                    {
                        let status = post_action_error_status(&error);
                        http_span.set_status(Status::error(error.clone()));
                        http_span.set_attribute(OtelKeyValue::new(
                            "http.status_code",
                            status.as_u16() as i64,
                        ));
                        return odata_error(status, "PostActionHookFailed", &error).into_response();
                    }
                }
                let body =
                    annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
                http_span.set_attribute(OtelKeyValue::new("idempotency.hit", true));
                http_span.set_status(Status::Ok);
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));
                let end_time: std::time::SystemTime = sim_now().into();
                http_span.end_with_timestamp(end_time);
                return ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response();
            }
            BoundActionClaim::Claimed => {
                dispatch_agent_ctx.idempotency_key = Some(durable_key);
                reservation_guard = Some(state.idempotency_cache.guard_bound_action_reservation(
                    &actor_key,
                    idem_key,
                    &request_fingerprint,
                ));
            }
        }
    }

    let result = state
        .dispatch_tenant_action_ext_typed(
            tenant,
            entity_type,
            key_str,
            action,
            body_json.clone(),
            DispatchExtOptions {
                agent_ctx: &dispatch_agent_ctx,
                await_integration,
                await_reactions: true,
            },
        )
        .await;

    let http_end: std::time::SystemTime = sim_now().into();
    let response = match result {
        Ok(response) => {
            if response.success {
                // Cache for idempotency
                if let Some(ref idem_key) = idempotency_key {
                    if !state.idempotency_cache.put_bound_action_effects_applied(
                        &actor_key,
                        idem_key,
                        response.clone(),
                        request_fingerprint.clone(),
                        body_json.clone(),
                    ) {
                        return odata_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "IdempotencyCapacityExceeded",
                            "The completed action could not reserve bounded replay state; retry later",
                        )
                        .into_response();
                    }
                    if let Some(guard) = reservation_guard.take() {
                        guard.disarm();
                    }
                }

                http_span.set_status(Status::Ok);
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 200i64));

                let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                let hook_result = if let Some(idem_key) = idempotency_key.as_deref() {
                    run_or_recover_bound_action_hook(
                        state,
                        tenant,
                        entity_type,
                        key_str,
                        action,
                        &body_json,
                        &mut state_json,
                        generation_lease,
                        &actor_key,
                        idem_key,
                        &operation_fingerprint,
                        &request_fingerprint,
                    )
                    .await
                } else {
                    apply_bound_action_hook(
                        state,
                        tenant,
                        entity_type,
                        key_str,
                        action,
                        &body_json,
                        &mut state_json,
                        generation_lease,
                        &request_fingerprint,
                    )
                    .await
                    .map(|_| ())
                };
                if let Err(error) = hook_result {
                    let status = post_action_error_status(&error);
                    http_span.set_status(Status::error(error.clone()));
                    http_span.set_attribute(OtelKeyValue::new(
                        "http.status_code",
                        status.as_u16() as i64,
                    ));
                    return odata_error(status, "PostActionHookFailed", &error).into_response();
                }
                hydrate_blob_refs_for_tenant(state, tenant, &mut state_json).await;
                let body =
                    annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
                ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response()
            } else {
                http_span.set_status(Status::error(response.error.clone().unwrap_or_default()));
                http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
                odata_error(
                    StatusCode::CONFLICT,
                    "ActionFailed",
                    &response.error.unwrap_or_else(|| "Action failed".into()),
                )
                .into_response()
            }
        }
        Err(DispatchError::Ungoverned(entity)) => {
            let reason = format!(
                "Entity type '{entity}' has no registered spec — actions are denied by default"
            );
            http_span.set_status(Status::error("EntityTypeNotGoverned"));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 404i64));
            odata_error(StatusCode::NOT_FOUND, "EntityTypeNotGoverned", &reason).into_response()
        }
        Err(DispatchError::AuthzDenied(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 403i64));
            odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response()
        }
        Err(DispatchError::QuotaExceeded(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 413i64));
            odata_error(StatusCode::PAYLOAD_TOO_LARGE, "StorageCapExceeded", &reason)
                .into_response()
        }
        Err(DispatchError::Conflict(reason)) => {
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 409i64));
            odata_error(StatusCode::CONFLICT, "Conflict", &reason).into_response()
        }
        // ADR-0048: transient exhaustion → 503 Retry-After so clients and
        // proxies back off instead of paging someone.
        Err(e @ DispatchError::Transient { .. }) => {
            let reason = e.to_string();
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
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
            http_span.set_status(Status::error("DispatchDeferred"));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 503i64));
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
            http_span.set_status(Status::error(reason.clone()));
            http_span.set_attribute(OtelKeyValue::new("http.status_code", 500i64));
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DispatchError", &reason).into_response()
        }
    };

    http_span.end_with_timestamp(http_end);
    response
}

#[expect(
    clippy::too_many_arguments,
    reason = "the durable hook receipt binds every governed request identity component"
)]
pub(super) async fn apply_bound_action_hook(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: &serde_json::Value,
    state_json: &mut serde_json::Value,
    generation_lease: Option<&TenantGenerationLease>,
    operation_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let Some(hook) = state.bound_action_hook.as_ref() else {
        return Ok(None);
    };
    let expected_generation = if hook.requires_generation_handoff(entity_type, action) {
        let lease = generation_lease.ok_or_else(|| {
            "publication-capable bound action is missing its tenant generation lease".to_string()
        })?;
        let expected_generation = lease.captured_generation();
        lease.release();
        Some(expected_generation)
    } else {
        None
    };
    let hook_output = hook
        .after_bound_action(BoundActionHookContext {
            state,
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            state_json,
            operation_id,
            expected_generation,
        })
        .await?;
    merge_bound_action_hook_output(state_json, hook_output.as_ref());
    Ok(hook_output)
}

#[expect(
    clippy::too_many_arguments,
    reason = "hook recovery verifies the complete durable request and operation identity"
)]
pub(super) async fn run_or_recover_bound_action_hook(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: &serde_json::Value,
    state_json: &mut serde_json::Value,
    generation_lease: Option<&TenantGenerationLease>,
    actor_key: &str,
    raw_idempotency_key: &str,
    operation_fingerprint: &str,
    request_fingerprint: &str,
) -> Result<(), String> {
    let hook_guard = state.idempotency_cache.guard_bound_action_hook(
        actor_key,
        raw_idempotency_key,
        request_fingerprint,
        || state.spec_publication_gated(tenant),
    );
    let (_raw_prefix, _operation_prefix, durable_idempotency_key) =
        bound_action_durable_idempotency_key(
            raw_idempotency_key,
            operation_fingerprint,
            request_fingerprint,
        );
    let receipt = hook_receipt::BoundActionHookReceipt::new(
        tenant,
        entity_type,
        entity_id,
        action,
        durable_idempotency_key.clone(),
        request_fingerprint.to_string(),
        false,
        None,
    );
    if let Some(receipt) = hook_receipt::load_bound_action_hook_receipt(state, &receipt).await?
        && receipt.is_completed()
    {
        let hook_output = receipt.hook_output().cloned();
        merge_bound_action_hook_output(state_json, hook_output.as_ref());
        if !state.idempotency_cache.complete_bound_action_hook(
            actor_key,
            raw_idempotency_key,
            request_fingerprint,
            hook_output,
        ) {
            return Err("completed durable post-action hook could not be cached".to_string());
        }
        hook_guard.disarm();
        return Ok(());
    }
    hook_receipt::persist_bound_action_hook_receipt(state, &receipt).await?;

    let hook_output = apply_bound_action_hook(
        state,
        tenant,
        entity_type,
        entity_id,
        action,
        params,
        state_json,
        generation_lease,
        &durable_idempotency_key,
    )
    .await?;
    let receipt = hook_receipt::BoundActionHookReceipt::new(
        tenant,
        entity_type,
        entity_id,
        action,
        durable_idempotency_key,
        request_fingerprint.to_string(),
        true,
        hook_output.clone(),
    );
    hook_receipt::persist_bound_action_hook_receipt(state, &receipt).await?;
    if !state.idempotency_cache.complete_bound_action_hook(
        actor_key,
        raw_idempotency_key,
        request_fingerprint,
        hook_output,
    ) {
        return Err("completed post-action hook could not be cached".to_string());
    }
    hook_guard.disarm();
    Ok(())
}

pub(super) fn merge_bound_action_hook_output(
    state_json: &mut serde_json::Value,
    hook_output: Option<&serde_json::Value>,
) {
    if let Some(hook_json) = hook_output
        && let (Some(dst), Some(src)) = (state_json.as_object_mut(), hook_json.as_object())
    {
        dst.insert(
            "postAction".to_string(),
            serde_json::Value::Object(src.clone()),
        );
    }
}

pub(super) fn post_action_error_status(error: &str) -> StatusCode {
    if error.contains("runtime generation is busy")
        || error.contains("advanced from runtime generation")
    {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
