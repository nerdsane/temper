use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use sha2::{Digest, Sha256};
use tokio::spawn as spawn_external_adapter_task; // determinism-ok: external adapter side effects
use tracing::{Instrument, instrument};

use crate::adapters::{
    AdapterAgentContext, AdapterContext, AdapterError, AdapterResult, AgentAdapter,
};
use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::identity::hash_token;
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::{
    WasmDispatchMode, WasmDispatchRequest, WasmEntityRef, record_workflow_span_attrs, retry,
};

const ADAPTER_INVOCATION_BUDGET_SECS: u64 = 60 * 60;
const ADAPTER_CREDENTIAL_TTL_SECS: i64 = 61 * 60;
const ADAPTER_CREDENTIAL_REVOKE_ATTEMPTS: usize = 3;

struct MintedAdapterCredential {
    plaintext: String,
    key_hash: String,
}

const REDACTED_ADAPTER_CREDENTIAL: &str = "[REDACTED_ADAPTER_CREDENTIAL]";
const REDACTED_ADAPTER_SECRET: &str = "[REDACTED_ADAPTER_SECRET]";

fn derive_adapter_credential_plaintext(first: uuid::Uuid, second: uuid::Uuid) -> String {
    let mut digest = Sha256::new();
    digest.update(b"temper-adapter-credential-v1\0");
    digest.update(first.as_bytes());
    digest.update(second.as_bytes());
    format!("tmpr_{:x}", digest.finalize())
}

fn adapter_redactions(ctx: &AdapterContext) -> Vec<(String, &'static str)> {
    let mut by_value = BTreeMap::new();
    for secret in ctx.secrets.values().filter(|secret| !secret.is_empty()) {
        by_value.insert(secret.clone(), REDACTED_ADAPTER_SECRET);
    }
    if let Some(credential) = ctx
        .agent_ctx
        .agent_api_key
        .as_ref()
        .filter(|credential| !credential.is_empty())
    {
        by_value.insert(credential.clone(), REDACTED_ADAPTER_CREDENTIAL);
    }
    let mut redactions = by_value.into_iter().collect::<Vec<_>>();
    redactions.sort_by(|left, right| {
        right
            .0
            .len()
            .cmp(&left.0.len())
            .then_with(|| left.0.cmp(&right.0))
    });
    redactions
}

fn redact_adapter_text(mut text: String, redactions: &[(String, &'static str)]) -> String {
    for (secret, replacement) in redactions {
        text = text.replace(secret, replacement);
    }
    text
}

fn redact_adapter_json(value: &mut serde_json::Value, redactions: &[(String, &'static str)]) {
    match value {
        serde_json::Value::String(text) => {
            *text = redact_adapter_text(std::mem::take(text), redactions);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_adapter_json(value, redactions);
            }
        }
        serde_json::Value::Object(fields) => {
            let prior = std::mem::take(fields);
            for (key, mut value) in prior {
                redact_adapter_json(&mut value, redactions);
                fields.insert(redact_adapter_text(key, redactions), value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_adapter_execution(
    execution: Result<AdapterResult, AdapterError>,
    redactions: &[(String, &'static str)],
) -> Result<AdapterResult, AdapterError> {
    if redactions.is_empty() {
        return execution;
    }
    match execution {
        Ok(mut result) => {
            if let Some(action) = result.callback_action.take() {
                result.callback_action = Some(redact_adapter_text(action, redactions));
            }
            if let Some(error) = result.error.take() {
                result.error = Some(redact_adapter_text(error, redactions));
            }
            redact_adapter_json(&mut result.callback_params, redactions);
            Ok(result)
        }
        Err(AdapterError::Invocation(error)) => Err(AdapterError::Invocation(redact_adapter_text(
            error, redactions,
        ))),
        Err(AdapterError::Execution(error)) => Err(AdapterError::Execution(redact_adapter_text(
            error, redactions,
        ))),
        Err(AdapterError::Parse(error)) => {
            Err(AdapterError::Parse(redact_adapter_text(error, redactions)))
        }
    }
}

async fn execute_adapter_with_budget(
    adapter: Arc<dyn AgentAdapter>,
    adapter_ctx: AdapterContext,
) -> Result<AdapterResult, AdapterError> {
    let redactions = adapter_redactions(&adapter_ctx);
    let execution = AssertUnwindSafe(tokio::time::timeout(
        Duration::from_secs(ADAPTER_INVOCATION_BUDGET_SECS),
        adapter.execute(adapter_ctx),
    ))
    .catch_unwind()
    .await;
    let execution = match execution {
        Ok(Ok(result)) => result,
        Ok(Err(_elapsed)) => Err(AdapterError::Execution(format!(
            "adapter invocation exceeded its {ADAPTER_INVOCATION_BUDGET_SECS}-second budget"
        ))),
        Err(_) => Err(AdapterError::Execution(
            "adapter invocation panicked".to_string(),
        )),
    };
    redact_adapter_execution(execution, &redactions)
}

struct AdapterDispatchCtx<'a> {
    entity_ref: WasmEntityRef<'a>,
    action: &'a str,
    agent_ctx: &'a AgentContext,
    mode: WasmDispatchMode,
}

pub(crate) struct AdapterDispatchInput<'a> {
    pub(crate) tenant: &'a TenantId,
    pub(crate) entity_type: &'a str,
    pub(crate) entity_id: &'a str,
    pub(crate) action: &'a str,
    pub(crate) custom_effects: &'a [String],
    pub(crate) entity_state: &'a EntityState,
    pub(crate) agent_ctx: &'a AgentContext,
    pub(crate) action_params: &'a serde_json::Value,
}

impl crate::state::ServerState {
    /// Dispatch native adapter integrations for custom effects in background mode.
    pub(crate) fn dispatch_adapter_integrations(&self, input: AdapterDispatchInput<'_>) {
        let state = self.clone();
        let tenant = input.tenant.clone();
        let entity_type = input.entity_type.to_string();
        let entity_id = input.entity_id.to_string();
        let action = input.action.to_string();
        let custom_effects = input.custom_effects.to_vec();
        let entity_state = input.entity_state.clone();
        let agent_ctx = input.agent_ctx.clone();
        let action_params = input.action_params.clone();
        let workflow_root_entity_type = agent_ctx
            .workflow_root_entity_type
            .clone()
            .unwrap_or_else(|| entity_type.clone());
        let workflow_root_entity_id = agent_ctx
            .workflow_root_entity_id
            .clone()
            .unwrap_or_else(|| entity_id.clone());
        let workflow_run_id = agent_ctx
            .workflow_run_id
            .clone()
            .unwrap_or_else(|| format!("{entity_type}:{entity_id}"));
        let span = tracing::info_span!(
            "dispatch.background_adapter_integrations",
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            temper.action = %action,
            entity_type = %entity_type,
            entity_id = %entity_id,
        );

        spawn_external_adapter_task(
            async move {
                // determinism-ok: async integration side-effects run outside simulation core
                let req = WasmDispatchRequest {
                    tenant: &tenant,
                    entity_type: &entity_type,
                    entity_id: &entity_id,
                    action: &action,
                    custom_effects: &custom_effects,
                    entity_state: &entity_state,
                    agent_ctx: &agent_ctx,
                    dispatch_idempotency_key: None,
                    action_params: &action_params,
                    mode: WasmDispatchMode::Background,
                };
                if let Err(e) = state.dispatch_adapter_integrations_internal(&req).await {
                    tracing::error!(error = %e, "background adapter integration dispatch failed");
                    // ADR-0152: compensate the durable transition with a
                    // failure transition; never a silent drop. Sync method that
                    // spawns its own task, breaking async recursion.
                    state.dispatch_integration_failure_compensation(
                        &tenant,
                        &entity_type,
                        &entity_id,
                        &action,
                        &e,
                    );
                }
            }
            .instrument(span),
        );
    }

    /// Dispatch adapter integrations in either inline or background mode.
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch_adapter_integrations_internal",
        tenant = %req.tenant,
        entity_type = req.entity_type,
        entity_id = req.entity_id,
        action_name = req.action,
        workflow.root_entity_type = tracing::field::Empty,
        workflow.root_entity_id = tracing::field::Empty,
        workflow.run_id = tracing::field::Empty,
        temper.action = tracing::field::Empty,
        session.id = tracing::field::Empty,
    ))]
    pub(crate) async fn dispatch_adapter_integrations_internal(
        &self,
        req: &WasmDispatchRequest<'_>,
    ) -> Result<Option<EntityResponse>, String> {
        record_workflow_span_attrs(
            req.agent_ctx,
            req.entity_type,
            req.entity_id,
            Some(req.action),
        );
        let integrations = {
            let registry = self
                .registry
                .read()
                .map_err(|e| format!("registry lock poisoned: {e}"))?;
            registry
                .get_spec(req.tenant, req.entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };

        let ctx = AdapterDispatchCtx {
            entity_ref: WasmEntityRef {
                tenant: req.tenant,
                entity_type: req.entity_type,
                entity_id: req.entity_id,
            },
            action: req.action,
            agent_ctx: req.agent_ctx,
            mode: req.mode,
        };

        let mut last_response: Option<EntityResponse> = None;

        for effect_name in req.custom_effects {
            let integration = integrations
                .iter()
                .find(|ig| ig.integration_type == "adapter" && ig.trigger == *effect_name)
                .cloned();
            let Some(integration) = integration else {
                continue;
            };

            if let Some(resp) = self
                .dispatch_single_adapter_integration(
                    &ctx,
                    &integration,
                    req.entity_state,
                    req.action_params,
                )
                .await?
            {
                last_response = Some(resp);
            }
        }

        Ok(last_response)
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_single_adapter_integration", integration = %integration.name))]
    async fn dispatch_single_adapter_integration(
        &self,
        ctx: &AdapterDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        entity_state: &EntityState,
        action_params: &serde_json::Value,
    ) -> Result<Option<EntityResponse>, String> {
        let adapter_type = entity_state
            .fields
            .get("adapter_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| integration.config.get("adapter").cloned())
            .or_else(|| integration.config.get("adapter_type").cloned())
            .ok_or_else(|| {
                format!(
                    "adapter integration '{}' is missing required config key 'adapter'",
                    integration.name
                )
            })?;

        let Some(adapter) = self.adapter_registry.get(&adapter_type) else {
            return self
                .handle_adapter_failure(
                    ctx,
                    integration,
                    format!("adapter '{adapter_type}' not found in registry"),
                    0,
                )
                .await;
        };

        let tenant = ctx.entity_ref.tenant.to_string();
        let integration_config = match self.secrets_vault.as_ref() {
            Some(vault) => resolve_secret_templates(&integration.config, vault, &tenant),
            None => integration.config.clone(),
        };
        let secrets = self
            .secrets_vault
            .as_ref()
            .map(|vault| vault.get_tenant_secrets(&tenant))
            .unwrap_or_default();

        // Mint a platform credential if the entity references an AgentType (ADR-0033).
        // The plaintext key is passed to the adapter and never persisted.
        let minted_credential = if adapter.requires_platform_credential() {
            self.mint_agent_credential_if_needed(ctx.entity_ref.tenant, entity_state, ctx.agent_ctx)
                .await?
        } else {
            None
        };
        let (agent_api_key, credential_key_hash) = match minted_credential {
            Some(credential) => (Some(credential.plaintext), Some(credential.key_hash)),
            None => (None, None),
        };

        let adapter_ctx = AdapterContext {
            tenant,
            entity_type: ctx.entity_ref.entity_type.to_string(),
            entity_id: ctx.entity_ref.entity_id.to_string(),
            trigger_action: ctx.action.to_string(),
            trigger_params: action_params.clone(),
            entity_state: serde_json::to_value(entity_state).unwrap_or_default(),
            integration_config,
            agent_ctx: AdapterAgentContext {
                agent_id: ctx.agent_ctx.agent_id.clone(),
                session_id: ctx.agent_ctx.session_id.clone(),
                agent_type: ctx.agent_ctx.agent_type.clone(),
                agent_api_key,
            },
            secrets,
        };

        let result = match self
            .execute_adapter_with_credential_cleanup(
                adapter,
                adapter_ctx,
                ctx.entity_ref.tenant,
                credential_key_hash,
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                return self
                    .handle_adapter_failure(ctx, integration, e.to_string(), 0)
                    .await;
            }
        };

        if result.success {
            let callback_action = integration
                .on_success
                .clone()
                .or_else(|| result.callback_action.clone());
            let Some(callback_action) = callback_action else {
                return Ok(None);
            };
            let callback_params = normalize_success_params(result);
            return self
                .dispatch_adapter_callback(
                    ctx.entity_ref,
                    &callback_action,
                    callback_params,
                    ctx.agent_ctx,
                    ctx.mode,
                )
                .await;
        }

        let error = result
            .error
            .clone()
            .unwrap_or_else(|| "adapter returned unsuccessful result".to_string());
        self.handle_adapter_failure(ctx, integration, error, result.duration_ms)
            .await
    }

    async fn execute_adapter_with_credential_cleanup(
        &self,
        adapter: Arc<dyn AgentAdapter>,
        adapter_ctx: AdapterContext,
        tenant: &TenantId,
        credential_key_hash: Option<String>,
    ) -> Result<AdapterResult, AdapterError> {
        let Some(credential_key_hash) = credential_key_hash else {
            return execute_adapter_with_budget(adapter, adapter_ctx).await;
        };
        let state = self.clone();
        let tenant = tenant.clone();
        let span = tracing::Span::current();

        // This task owns both execution and cleanup. Dropping the caller's
        // JoinHandle detaches rather than cancels it, so request cancellation
        // cannot skip durable credential revocation.
        let cleanup_task = spawn_external_adapter_task(
            async move {
                // determinism-ok: native adapter execution is an external side effect
                let execution = execute_adapter_with_budget(adapter, adapter_ctx).await;
                if let Err(error) = state
                    .revoke_minted_adapter_credential(&tenant, &credential_key_hash)
                    .await
                {
                    tracing::error!(
                        tenant = %tenant,
                        adapter_execution_completed = execution.is_ok(),
                        credential_ttl_secs = ADAPTER_CREDENTIAL_TTL_SECS,
                        error = %error,
                        "adapter credential cleanup exhausted its retry budget; preserving the adapter execution outcome"
                    );
                }
                execution
            }
            .instrument(span),
        );

        cleanup_task.await.map_err(|error| {
            AdapterError::Execution(format!("adapter cleanup task failed: {error}"))
        })?
    }

    async fn revoke_minted_adapter_credential(
        &self,
        tenant: &TenantId,
        key_hash: &str,
    ) -> Result<(), String> {
        let actor = self
            .get_or_spawn_tenant_actor(tenant, "AgentCredential", key_hash)
            .ok_or_else(|| "AgentCredential transition table is unavailable".to_string())?;
        let policy = self.dispatch_retry_policy();
        let idempotency_key = format!("adapter-credential-revoke:{key_hash}");
        let mut last_error = "credential remained Active".to_string();

        for attempt in 1..=ADAPTER_CREDENTIAL_REVOKE_ATTEMPTS {
            let outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
                &actor,
                || EntityMsg::Action {
                    name: "Revoke".to_string(),
                    params: serde_json::json!({}),
                    cross_entity_booleans: BTreeMap::new(),
                    idempotency_key: Some(idempotency_key.clone()),
                    expected_sequence: None,
                    reaction_context: None,
                    expected_authorization_precondition: None,
                },
                &policy,
            )
            .await;
            match outcome.result {
                Ok(response) if response.success || response.state.status != "Active" => {
                    return Ok(());
                }
                Ok(response) => {
                    last_error = response
                        .error
                        .unwrap_or_else(|| "credential remained Active".to_string());
                }
                Err(error) => {
                    last_error = error.to_string();
                }
            }
            tracing::warn!(
                tenant = %tenant,
                attempt,
                max_attempts = ADAPTER_CREDENTIAL_REVOKE_ATTEMPTS,
                error = %last_error,
                "adapter credential revocation attempt failed"
            );
        }

        Err(last_error)
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.handle_adapter_failure", integration = %integration.name))]
    async fn handle_adapter_failure(
        &self,
        ctx: &AdapterDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        error: String,
        duration_ms: u64,
    ) -> Result<Option<EntityResponse>, String> {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_type = ctx.entity_ref.entity_type,
            entity_id = ctx.entity_ref.entity_id,
            integration = %integration.name,
            error = %error,
            "adapter integration failed"
        );

        let Some(callback_action) = integration.on_failure.clone() else {
            // No declared recovery: propagate the failure instead of swallowing
            // it (ADR-0152). Inline this surfaces as `success: false`;
            // background the dispatcher drives a compensating transition.
            return Err(error);
        };

        let params = serde_json::json!({
            "error": error,
            "error_message": error,
            "integration": integration.name,
            "duration_ms": duration_ms,
        });

        self.dispatch_adapter_callback(
            ctx.entity_ref,
            &callback_action,
            params,
            ctx.agent_ctx,
            ctx.mode,
        )
        .await
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_adapter_callback", callback_action))]
    async fn dispatch_adapter_callback(
        &self,
        entity_ref: WasmEntityRef<'_>,
        callback_action: &str,
        callback_params: serde_json::Value,
        agent_ctx: &AgentContext,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        match mode {
            WasmDispatchMode::Inline => {
                let resp = self
                    .dispatch_tenant_action_core(
                        entity_ref.tenant,
                        entity_ref.entity_type,
                        entity_ref.entity_id,
                        callback_action,
                        callback_params,
                        agent_ctx,
                        false,
                        None,
                        None,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                let callback_ctx =
                    AgentContext::for_service_inheriting("platform-dispatch", agent_ctx);
                self.dispatch_tenant_action(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &callback_ctx,
                )
                .await
                .map_err(|e| {
                    let msg =
                        format!("failed to dispatch adapter callback '{callback_action}': {e}");
                    tracing::error!(callback = %callback_action, error = %e, "{msg}");
                    msg
                })?;
                Ok(None)
            }
        }
    }

    /// Mint a platform credential for adapter execution if the entity has an `agent_type_id`.
    ///
    /// Generates a random API key, hashes it, creates an `AgentCredential` entity
    /// via the `Issue` action, and returns the plaintext key. The full key is
    /// never persisted or logged; only its hash and prefix are durable. The
    /// credential has a bounded expiry and is revoked after execution.
    ///
    /// See ADR-0033: Platform-Assigned Agent Identity.
    async fn mint_agent_credential_if_needed(
        &self,
        tenant: &TenantId,
        entity_state: &EntityState,
        agent_ctx: &AgentContext,
    ) -> Result<Option<MintedAdapterCredential>, String> {
        let agent_type_id = entity_state
            .fields
            .get("agent_type_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let Some(agent_type_id) = agent_type_id else {
            return Ok(None);
        };

        // UUIDv7 contains only 74 random bits. Derive the opaque credential
        // from two independent scheduler-provided UUIDs so production exceeds
        // the 128-bit entropy bar while DST retains deterministic sources.
        let plaintext_key = derive_adapter_credential_plaintext(sim_uuid(), sim_uuid());
        let key_hash = hash_token(&plaintext_key);
        let key_prefix = &plaintext_key[..9]; // "tmpr_" + first four digest characters
        let agent_instance_id = sim_uuid().to_string();
        let expires_at = sim_now()
            .checked_add_signed(chrono::Duration::seconds(ADAPTER_CREDENTIAL_TTL_SECS))
            .ok_or_else(|| "adapter credential expiry overflowed".to_string())?
            .to_rfc3339();

        let issue_params = serde_json::json!({
            "agent_type_id": agent_type_id,
            "agent_instance_id": agent_instance_id,
            "key_hash": key_hash,
            "key_prefix": key_prefix,
            "description": "Auto-minted for adapter invocation",
            "created_by": "platform",
            "expires_at": expires_at,
        });

        // Credential issuance is a separate authority from permission to run
        // the source entity action. Re-evaluate the exact invoking principal
        // against the credential resource so a low-privilege action cannot use
        // the platform as a deputy to mint a more privileged AgentType token.
        let security_ctx = agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            "adapter credential mint requires an explicit security context".to_string()
        })?;
        let credential_attrs = BTreeMap::from([
            (
                "id".to_string(),
                serde_json::Value::String(key_hash.clone()),
            ),
            (
                "agent_type_id".to_string(),
                serde_json::Value::String(agent_type_id.to_string()),
            ),
            (
                "agent_instance_id".to_string(),
                serde_json::Value::String(agent_instance_id.clone()),
            ),
            (
                "expires_at".to_string(),
                serde_json::Value::String(expires_at.clone()),
            ),
        ]);
        self.authorize_with_context(
            security_ctx,
            "Issue",
            "AgentCredential",
            &credential_attrs,
            tenant.as_str(),
        )
        .map_err(|denial| format!("adapter credential delegation denied: {denial}"))?;

        // Create the AgentCredential entity using key_hash as entity ID for O(1) lookup.
        let dispatch_ctx = AgentContext::for_service_inheriting("platform-dispatch", agent_ctx);
        let result = self
            .dispatch_tenant_action(
                tenant,
                "AgentCredential",
                &key_hash,
                "Issue",
                issue_params,
                &dispatch_ctx,
            )
            .await;

        match result {
            Ok(resp) if resp.success => {
                tracing::info!(
                    tenant = %tenant,
                    agent_type_id = agent_type_id,
                    agent_instance_id = %agent_instance_id,
                    key_prefix = key_prefix,
                    "minted agent credential for adapter execution"
                );
                Ok(Some(MintedAdapterCredential {
                    plaintext: plaintext_key,
                    key_hash,
                }))
            }
            Ok(resp) => Err(format!(
                "failed to mint required adapter credential: {}",
                resp.error
                    .unwrap_or_else(|| "Issue action was rejected".to_string())
            )),
            Err(error) => Err(format!(
                "failed to mint required adapter credential: {error}"
            )),
        }
    }
}

fn normalize_success_params(result: AdapterResult) -> serde_json::Value {
    let mut callback_params = result.callback_params;
    match callback_params {
        serde_json::Value::Object(ref mut obj) => {
            obj.entry("duration_ms".to_string())
                .or_insert(serde_json::json!(result.duration_ms));
            if let Some(error) = result.error {
                obj.entry("adapter_error".to_string())
                    .or_insert(serde_json::json!(error));
            }
            callback_params
        }
        _ => serde_json::json!({
            "result": callback_params,
            "duration_ms": result.duration_ms,
        }),
    }
}

#[cfg(test)]
#[path = "adapter_credential_test.rs"]
mod credential_tests;
