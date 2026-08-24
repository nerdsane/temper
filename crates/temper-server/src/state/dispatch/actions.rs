use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{Instrument, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::entity_actor::{EntityMsg, EntityResponse};
use crate::request_context::{AgentContext, remote_parent_context};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::effects::PostDispatchContext;
use super::retry;
use super::{DispatchCommand, DispatchError, DispatchExtOptions, record_workflow_span_attrs};
use crate::state::admission::AdmissionOutcome;

const DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY: usize = 10;

struct BackgroundReactionDispatch {
    dispatcher: Arc<crate::trigger::ReactionDispatcher>,
    tenant: TenantId,
    intents: Vec<crate::trigger::delivery::PersistedReactionIntent>,
}

fn background_reaction_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(
        SEMAPHORE
            .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY))),
    )
}

/// Maximum request-body length, in bytes, echoed into the dispatch-failure log.
const LOG_REQUEST_BODY_MAX_BYTES: usize = 4096;

/// Bound a serialized request body for the failure log line.
///
/// Slicing at a fixed byte offset panics when the cap lands inside a multi-byte
/// character, which turns a logged dispatch failure into a second failure. The
/// cut walks back to the nearest character boundary instead.
fn truncate_request_body_for_log(serialized: &str) -> String {
    if serialized.len() <= LOG_REQUEST_BODY_MAX_BYTES {
        return serialized.to_string();
    }
    let mut end = LOG_REQUEST_BODY_MAX_BYTES;
    while end > 0 && !serialized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &serialized[..end])
}

impl crate::state::ServerState {
    /// Dispatch an action using the unified command object.
    ///
    /// This is the preferred entry point. The command struct makes all
    /// parameters explicit (especially tenant) and avoids the previous
    /// three-layer wrapper chain.
    #[instrument(skip_all, fields(
        otel.name = %format_args!("{}.{}", cmd.entity_type, cmd.action),
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
    ))]
    pub async fn dispatch(&self, cmd: DispatchCommand<'_>) -> Result<EntityResponse, String> {
        self.dispatch_typed(cmd).await.map_err(|e| e.to_string())
    }

    /// Dispatch an action to an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `dispatch(DispatchCommand { .. })` with explicit tenant")]
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) for the common
    /// case where `await_integration` is `false`.
    ///
    /// Callers that need integration await or other options should use
    /// `dispatch(DispatchCommand { .. })` directly.
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.dispatch(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) with full options.
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action_ext(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action_ext_typed(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            options,
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// Typed variant of [`dispatch_tenant_action_ext`](Self::dispatch_tenant_action_ext).
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action_ext_typed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx: options.agent_ctx,
            await_integration: options.await_integration,
            await_reactions: options.await_reactions,
        })
        .await
    }

    /// Dispatch an externally authorized action only if the target actor still
    /// matches the exact local state used for the Cedar decision.
    #[instrument(skip_all, fields(
        otel.name = %format_args!("{}.{}", cmd.entity_type, cmd.action),
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
    ))]
    pub(crate) async fn dispatch_tenant_action_ext_typed_if_current(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, Some(expected_authorization_precondition))
            .await
    }

    async fn dispatch_typed(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, None).await
    }

    async fn dispatch_typed_checked(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, DispatchError> {
        let DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration,
            await_reactions,
        } = cmd;

        if self
            .composite_metadata_for(tenant, entity_type, action, agent_ctx.schema_pin.as_ref())?
            .is_some()
        {
            self.reject_action_supplied_sub_writes(entity_type, action, &params)?;
        }

        let durable_timeouts_expected = if self.event_journal().is_some() {
            let registry = self
                .registry
                .read()
                .map_err(|_| DispatchError::Internal("registry lock poisoned".into()))?;
            match agent_ctx.schema_pin.as_ref() {
                Some(pin) => registry.get_scoped_spec_at_digest(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_type,
                ),
                None => registry.get_spec(tenant, entity_type),
            }
            .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
        } else {
            false
        };
        let dispatcher = self
            .reaction_dispatcher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let reaction_context = if let Some(dispatcher) = dispatcher.as_ref() {
            let rules = if let Some(pin) = agent_ctx.schema_pin.as_ref() {
                self.registry
                    .read()
                    .map_err(|_| DispatchError::Internal("registry lock poisoned".into()))?
                    .scoped_reaction_candidates_at_digest(
                        tenant,
                        &pin.scope,
                        &pin.bundle_digest,
                        entity_type,
                        action,
                    )
            } else {
                dispatcher.candidate_rules(tenant, entity_type, action)
            };
            if rules.is_empty() && !durable_timeouts_expected {
                None
            } else {
                let guard_source = match agent_ctx.schema_pin.as_ref() {
                    Some(pin) => self
                        .get_or_initialize_scoped_entity_state(
                            tenant,
                            entity_type,
                            entity_id,
                            pin.clone(),
                        )
                        .await
                        .map_err(DispatchError::Internal)?,
                    None => self
                        .get_tenant_entity_state(tenant, entity_type, entity_id)
                        .await
                        .map_err(DispatchError::Internal)?,
                };
                let expected_source_sequence = guard_source.state.sequence_nr;
                let mut guard_fields = guard_source.state.fields;
                if let (Some(fields), Some(action_params)) =
                    (guard_fields.as_object_mut(), params.as_object())
                {
                    for (name, value) in action_params {
                        fields.insert(name.clone(), value.clone());
                    }
                }
                let resolved_guards = crate::trigger::dispatcher::resolve_rule_guard_inputs(
                    self,
                    tenant,
                    &rules,
                    &guard_fields,
                    agent_ctx.schema_pin.as_ref(),
                )
                .await;
                let authority = serde_json::to_value(
                    crate::trigger::dispatcher::effective_trigger_security_context(agent_ctx),
                )
                .map_err(|error| DispatchError::Internal(error.to_string()))?;
                Some(crate::trigger::delivery::ReactionCommitContext {
                    rules,
                    authority,
                    depth: 0,
                    root_delivery_id: None,
                    expected_source_sequence,
                    resolved_guards,
                    receipt: None,
                })
            }
        } else {
            None
        };

        let durable_reactions_expected = reaction_context
            .as_ref()
            .is_some_and(|context| !context.rules.is_empty());
        let durable_intents_expected = durable_reactions_expected || durable_timeouts_expected;
        if durable_reactions_expected && self.event_journal().is_none() {
            return Err(DispatchError::Internal(
                "durable reactions require a configured event journal".to_string(),
            ));
        }
        let response = self
            .dispatch_tenant_action_core(
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
                reaction_context,
                expected_authorization_precondition,
            )
            .await?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success
            && let Some(dispatcher) = dispatcher
        {
            // Keep the durable recovery/cascade future off actor worker stacks.
            // Its bounded scans and fanout state are materially larger than a
            // normal action future, especially when WASM callbacks nest a
            // second dispatch on the same worker.
            Box::pin(async {
                let to_state = response.state.status.clone();
                let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
                let committed_intents = if durable_intents_expected {
                    self.materialize_committed_reaction_intents(
                        tenant,
                        entity_type,
                        entity_id,
                        response.state.sequence_nr,
                        agent_ctx.schema_pin.as_ref(),
                    )
                    .await?
                } else {
                    Vec::new()
                };
                let had_committed_intents = !committed_intents.is_empty();
                let (reaction_intents, timeout_intents): (Vec<_>, Vec<_>) =
                    committed_intents.into_iter().partition(|intent| {
                        intent.kind == crate::trigger::delivery::DeliveryKind::Reaction
                    });
                if !timeout_intents.is_empty() {
                    // A state timeout is future scheduler work, never part of
                    // the caller's synchronous reaction-tree wait budget.
                    dispatcher.notify_recovery(tenant);
                }
                if !reaction_intents.is_empty() && await_reactions {
                    let root_delivery_ids: BTreeSet<_> = reaction_intents
                        .iter()
                        .map(|intent| intent.root_delivery_id.clone())
                        .collect();
                    for intent in reaction_intents {
                        dispatcher
                            .dispatch_committed_intent(self, intent)
                            .await
                            .map_err(DispatchError::Internal)?;
                    }
                    dispatcher
                        .drain_tenant_deliveries(
                            self,
                            tenant,
                            1_024,
                            std::time::Duration::from_secs(5),
                        )
                        .await
                        .map_err(DispatchError::Internal)?;
                    self.require_terminal_reaction_roots(tenant, &root_delivery_ids)
                        .await?;
                } else if !reaction_intents.is_empty() {
                    dispatcher.notify_recovery(tenant);
                    match background_reaction_semaphore().try_acquire_owned() {
                        Ok(permit) => self.spawn_background_reactions(
                            BackgroundReactionDispatch {
                                dispatcher: Arc::clone(&dispatcher),
                                tenant: tenant.clone(),
                                intents: reaction_intents,
                            },
                            permit,
                        ),
                        Err(error) => tracing::debug!(
                            tenant = %tenant,
                            %error,
                            "reaction intent is durable; periodic recovery will claim it"
                        ),
                    }
                } else if !had_committed_intents {
                    dispatcher
                        .dispatch_reactions(
                            self,
                            tenant,
                            entity_type,
                            entity_id,
                            action,
                            &to_state,
                            &fields,
                            0,
                            // ADR-0046: thread the invoking context through so
                            // reactions without an explicit principal inherit
                            // the invoking authority, and declared principals
                            // can elevate deterministically.
                            agent_ctx,
                        )
                        .await;
                }
                Ok::<(), DispatchError>(())
            })
            .await?;
        }

        // Scheduled actions are handled inside run_post_dispatch_effects
        // (called from dispatch_tenant_action_core).
        Ok(response)
    }

    /// Make every reaction intent on one committed source event durably pending.
    pub(crate) async fn materialize_committed_reaction_intents(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        source_sequence: u64,
        schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    ) -> Result<Vec<crate::trigger::delivery::PersistedReactionIntent>, DispatchError> {
        let (store, _) = self.event_journal().ok_or_else(|| {
            DispatchError::Internal(
                "durable reactions require a configured event journal".to_string(),
            )
        })?;
        if source_sequence == 0 {
            return Err(DispatchError::Internal(
                "successful persisted action returned sequence zero".to_string(),
            ));
        }
        let persistence_id = match schema_pin {
            Some(pin) => format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    entity_id, pin,
                )
            ),
            None => format!("{tenant}:{entity_type}:{entity_id}"),
        };
        let events = store
            .read_events_limited(&persistence_id, source_sequence - 1, 1)
            .await
            .map_err(|error| DispatchError::Internal(error.to_string()))?;
        let source_event = events
            .iter()
            .find(|event| event.sequence_nr == source_sequence)
            .ok_or_else(|| {
                DispatchError::Internal(format!(
                    "committed source event {persistence_id}@{source_sequence} was not readable"
                ))
            })?;
        let intents = crate::trigger::delivery::extract_intents(&source_event.payload)
            .map_err(DispatchError::Internal)?;
        for intent in &intents {
            crate::trigger::delivery::initialize_delivery_record(&store, intent.clone())
                .await
                .map_err(|error| DispatchError::Internal(error.to_string()))?;
        }
        Ok(intents)
    }

    async fn require_terminal_reaction_roots(
        &self,
        tenant: &TenantId,
        root_delivery_ids: &BTreeSet<String>,
    ) -> Result<(), DispatchError> {
        use crate::trigger::delivery::{ReactionDeliveryStatus, list_delivery_records};

        let Some((store, _)) = self.event_journal() else {
            return Err(DispatchError::Internal(
                "awaited durable reactions require an event journal".to_string(),
            ));
        };
        const DELIVERY_INSPECTION_BUDGET: usize = 10_000;
        let records = list_delivery_records(
            &store,
            tenant.as_str(),
            DELIVERY_INSPECTION_BUDGET.saturating_add(1),
        )
        .await
        .map_err(|error| DispatchError::Internal(error.to_string()))?;
        if records.len() > DELIVERY_INSPECTION_BUDGET {
            return Err(DispatchError::Internal(format!(
                "awaited reaction tree inspection exceeded the {DELIVERY_INSPECTION_BUDGET}-delivery budget"
            )));
        }
        for root_delivery_id in root_delivery_ids {
            let tree: Vec<_> = records
                .iter()
                .map(|(record, _)| record)
                .filter(|record| record.intent.root_delivery_id == *root_delivery_id)
                .collect();
            if tree.is_empty() {
                return Err(DispatchError::Internal(format!(
                    "awaited reaction tree {root_delivery_id} was not readable"
                )));
            }
            if let Some(failed) = tree.iter().find(|record| {
                matches!(
                    record.status,
                    ReactionDeliveryStatus::Rejected | ReactionDeliveryStatus::DeadLettered
                )
            }) {
                return Err(DispatchError::Internal(format!(
                    "reaction delivery {} reached terminal status {:?}",
                    failed.intent.delivery_id, failed.status
                )));
            }
            if let Some(incomplete) = tree.iter().find(|record| {
                matches!(
                    record.status,
                    ReactionDeliveryStatus::Pending
                        | ReactionDeliveryStatus::Claimed
                        | ReactionDeliveryStatus::Dispatching
                )
            }) {
                return Err(DispatchError::Internal(format!(
                    "reaction delivery {} is incomplete in status {:?}",
                    incomplete.intent.delivery_id, incomplete.status
                )));
            }
        }
        Ok(())
    }

    fn spawn_background_reactions(
        &self,
        dispatch: BackgroundReactionDispatch,
        permit: OwnedSemaphorePermit,
    ) {
        let state = self.clone();
        let BackgroundReactionDispatch {
            dispatcher,
            tenant,
            intents,
        } = dispatch;
        let span = tracing::info_span!(
            "reaction.dispatch.background",
            tenant = %tenant,
            reaction.intent_count = intents.len(),
        );

        let reaction_task = async move {
            let _permit = permit;
            for intent in intents {
                match dispatcher.dispatch_committed_intent(&state, intent).await {
                    Ok(_) => {}
                    Err(error) => tracing::error!(tenant = %tenant, %error, "durable reaction delivery failed"),
                }
            }
            tracing::info!(tenant = %tenant, "background reaction intents dispatched");
        }
        .instrument(span);
        tokio::spawn(reaction_task); // determinism-ok: production-only post-commit reaction side effects
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch_tenant_action_core",
        tenant = %tenant,
        entity_type,
        entity_id,
        action_name = action,
        workflow.root_entity_type = tracing::field::Empty,
        workflow.root_entity_id = tracing::field::Empty,
        workflow.run_id = tracing::field::Empty,
        temper.action = tracing::field::Empty,
        session.id = tracing::field::Empty,
        session_id = tracing::field::Empty,
        intent = tracing::field::Empty,
        observation_metadata = tracing::field::Empty,
        success = tracing::field::Empty,
        error_msg = tracing::field::Empty,
    ))]
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
        await_integration: bool,
        reaction_context: Option<crate::trigger::delivery::ReactionCommitContext>,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, DispatchError> {
        let explicit_workflow_context = agent_ctx.workflow_run_id.is_some()
            || agent_ctx.workflow_root_entity_type.is_some()
            || agent_ctx.workflow_root_entity_id.is_some();
        let use_workflow_root = crate::workflow_tracing::should_use_workflow_root_span(
            explicit_workflow_context,
            entity_type,
        );
        let mut enriched_agent_ctx = agent_ctx.for_dispatch_root(entity_type, entity_id);
        let mut workflow_root_active = false;
        if use_workflow_root {
            let root_entity_type = enriched_agent_ctx
                .workflow_root_entity_type
                .as_deref()
                .unwrap_or(entity_type);
            let root_entity_id = enriched_agent_ctx
                .workflow_root_entity_id
                .as_deref()
                .unwrap_or(entity_id);
            let workflow_run_id = enriched_agent_ctx
                .workflow_run_id
                .as_deref()
                .unwrap_or(root_entity_id);
            if let Some(parent) = self.workflow_spans.parent_context(
                tenant.as_str(),
                root_entity_type,
                root_entity_id,
                workflow_run_id,
                enriched_agent_ctx.session_id.as_deref(),
            ) {
                workflow_root_active = true;
                tracing::Span::current().set_parent(parent);
            }
        } else if let Some(parent) = remote_parent_context(agent_ctx) {
            tracing::Span::current().set_parent(parent);
        }
        enriched_agent_ctx = enriched_agent_ctx.with_current_span_trace_context();
        let agent_ctx = &enriched_agent_ctx;
        record_workflow_span_attrs(agent_ctx, entity_type, entity_id, Some(action));
        let current_span = tracing::Span::current();
        current_span.record("session_id", agent_ctx.session_id.as_deref().unwrap_or(""));
        current_span.record("intent", agent_ctx.intent.as_deref().unwrap_or(""));
        let observation_metadata = agent_ctx.observation_metadata_json().unwrap_or_default();
        current_span.record("observation_metadata", observation_metadata.as_str());
        if let Some(pin) = agent_ctx.schema_pin.as_ref() {
            self.validate_scoped_entity_pin_for_dispatch(tenant, entity_type, entity_id, pin)
                .await
                .map_err(DispatchError::Internal)?;
        }
        let entity_type_governed = if let Some(pin) = agent_ctx.schema_pin.as_ref() {
            self.registry
                .read()
                .map_err(|_| DispatchError::Internal("registry lock poisoned".to_string()))?
                .get_scoped_table_at_digest(tenant, &pin.scope, &pin.bundle_digest, entity_type)
                .is_some()
        } else {
            self.is_entity_type_governed(tenant, entity_type)
                .map_err(DispatchError::Internal)?
        };
        if !entity_type_governed {
            // Default-deny: entity type has no registered spec.
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                action,
                "rejecting action on ungoverned entity type (no spec registered)"
            );
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                action,
                source = "Entity",
                authz_denied = false,
                "unmet_intent"
            );
            return Err(DispatchError::Ungoverned(entity_type.to_string()));
        }

        // W2 phase: actor_spawn — registry lookup / actor-creation path.
        // Emitted as a child span so `aggregate_spans group by resource_name`
        // slices dispatch latency by phase cleanly.
        let (actor_ref, actor_existed_before) = {
            let _phase = tracing::info_span!(
                "dispatch.phase.actor_spawn",
                tenant = %tenant,
                entity_type,
                entity_id,
            )
            .entered();
            let registry_start = std::time::Instant::now(); // determinism-ok: wall-clock latency metric only, not on simulation path
            let actor_key = match agent_ctx.schema_pin.as_ref() {
                Some(pin) => format!(
                    "{tenant}:{entity_type}:{}",
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        entity_id, pin,
                    )
                ),
                None => format!("{tenant}:{entity_type}:{entity_id}"),
            };
            let existed = self
                .actor_registry
                .read()
                .map(|reg| reg.contains_key(&actor_key))
                .unwrap_or(false);
            let actor = match agent_ctx.schema_pin.as_ref() {
                Some(pin) => self.get_or_spawn_scoped_actor_with_fields(
                    tenant,
                    entity_type,
                    entity_id,
                    serde_json::json!({}),
                    pin.clone(),
                ),
                None => self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id),
            };
            let Some(ar) = actor else {
                return Err(DispatchError::Internal(format!(
                    "failed to resolve actor for governed entity type '{entity_type}'"
                )));
            };
            crate::runtime_metrics::record_actor_registry_lock_wait(
                entity_type,
                !existed,
                registry_start.elapsed(),
            );
            (ar, existed)
        };
        let cold_start_outcome_start = if !actor_existed_before {
            Some(std::time::Instant::now()) // determinism-ok: wall-clock latency metric only, not on simulation path
        } else {
            None
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let mut cross_entity_booleans = self
            .resolve_cross_entity_guards(
                tenant,
                entity_type,
                entity_id,
                action,
                agent_ctx.schema_pin.as_ref(),
            )
            .await;
        cross_entity_booleans.extend(
            self.resolve_reference_evidence(
                tenant,
                entity_type,
                entity_id,
                Some(action),
                &params,
                agent_ctx.schema_pin.as_ref(),
            )
            .await,
        );

        let action_params = params.clone();
        // ADR-0051: acquire an admission permit before spending retry budget.
        // Caps are pulled inline from the spec registry so `[admission]`
        // declarations take effect at spec load with no separate
        // registration step.
        // W2 phase: admission_acquire observability span (upstream main).
        let admission_span = tracing::info_span!(
            "dispatch.phase.admission_acquire",
            tenant = %tenant,
            entity_type,
            action_name = action,
        );
        let admission_start = std::time::Instant::now(); // determinism-ok: wall-clock latency metric only, not on simulation path
        let admission_caps = self.admission_caps_for(tenant, entity_type);
        let admission_was_capped = admission_caps.is_some();
        let admission_result = {
            use tracing::Instrument;
            self.admission
                .try_acquire_with_caps(tenant, entity_type, action, admission_caps.as_ref())
                .instrument(admission_span)
                .await
        };
        let _admission_permit = match admission_result {
            AdmissionOutcome::Passthrough => None,
            AdmissionOutcome::Granted(permit) => {
                let waited = admission_start.elapsed();
                crate::runtime_metrics::record_admission_granted(
                    tenant.as_str(),
                    entity_type,
                    action,
                    waited,
                );
                if waited > std::time::Duration::from_millis(1) {
                    crate::runtime_metrics::record_admission_queued(
                        tenant.as_str(),
                        entity_type,
                        action,
                    );
                }
                Some(permit)
            }
            AdmissionOutcome::Deferred {
                retry_after_ms,
                waited,
            } => {
                crate::runtime_metrics::record_admission_deferred(
                    tenant.as_str(),
                    entity_type,
                    action,
                    waited,
                );
                crate::runtime_metrics::record_dispatch_outcome(
                    tenant.as_str(),
                    entity_type,
                    action,
                    crate::runtime_metrics::DispatchOutcome::Deferred,
                    0,
                    admission_start.elapsed(),
                );
                return Err(DispatchError::Deferred { retry_after_ms });
            }
        };
        let _ = admission_was_capped; // reserved for active-permits gauge
        // ADR-0048: wrap the ask in a bounded retry that classifies
        // transient (AskTimeout / MailboxFull) vs permanent failures.
        let mut policy = self.dispatch_retry_policy();
        if expected_authorization_precondition.is_some() {
            policy.max_attempts = 1;
        }
        let action_name = action.to_string();
        let params_for_retry = params;
        let cross_for_retry = cross_entity_booleans;
        let reactions_for_retry = reaction_context;
        let reaction_expected_sequence = reactions_for_retry
            .as_ref()
            .map(|context| context.expected_source_sequence);
        let authorization_precondition_for_retry = expected_authorization_precondition;
        let idempotency_key = Some(agent_ctx.idempotency_key.clone().unwrap_or_else(|| {
            format!(
                "dispatch:{tenant}:{entity_type}:{entity_id}:{action}:{}",
                sim_uuid()
            )
        }));
        // W2 phase: ask_reply — round-trip through the retry layer to the
        // actor and back. Aggregates with other phase spans by prefix.
        let ask_span = tracing::info_span!(
            "dispatch.phase.ask_reply",
            tenant = %tenant,
            entity_type,
            action_name = %action_name,
        );
        let outcome = {
            use tracing::Instrument;
            retry::ask_with_backoff::<_, EntityResponse, _>(
                &actor_ref,
                || EntityMsg::Action {
                    name: action_name.clone(),
                    params: params_for_retry.clone(),
                    cross_entity_booleans: cross_for_retry.clone(),
                    idempotency_key: idempotency_key.clone(),
                    expected_sequence: agent_ctx
                        .expected_entity_sequence
                        .or(reaction_expected_sequence),
                    reaction_context: reactions_for_retry.clone().map(Box::new),
                    expected_authorization_precondition: authorization_precondition_for_retry
                        .clone(),
                },
                &policy,
            )
            .instrument(ask_span)
            .await
        };
        // ADR-0048: emit dispatch outcome / attempts / latency metrics.
        let ask_outcome_for_metrics = match &outcome.result {
            Ok(_) => {
                if outcome.retried_after_transient {
                    crate::runtime_metrics::DispatchOutcome::TransientRetriedOk
                } else {
                    crate::runtime_metrics::DispatchOutcome::Ok
                }
            }
            Err(e) => {
                let kind: &'static str = match e {
                    temper_runtime::actor::ActorError::AskTimeout(_) => "ask_timeout",
                    temper_runtime::actor::ActorError::MailboxFull => "mailbox_full",
                    temper_runtime::actor::ActorError::Stopped => "stopped",
                    temper_runtime::actor::ActorError::SendFailed => "send_failed",
                    temper_runtime::actor::ActorError::Panicked(_) => "panicked",
                    temper_runtime::actor::ActorError::InitFailed(_) => "init_failed",
                    temper_runtime::actor::ActorError::MaxRestartsExceeded(_) => {
                        "max_restarts_exceeded"
                    }
                    temper_runtime::actor::ActorError::Custom(_) => "custom",
                };
                crate::runtime_metrics::record_dispatch_error(
                    tenant.as_str(),
                    entity_type,
                    action,
                    kind,
                );
                if e == &temper_runtime::actor::ActorError::MailboxFull {
                    crate::runtime_metrics::record_actor_mailbox_full_drop(entity_type, action);
                }
                if e.is_transient() {
                    crate::runtime_metrics::DispatchOutcome::TransientExhausted
                } else {
                    crate::runtime_metrics::DispatchOutcome::Permanent
                }
            }
        };
        crate::runtime_metrics::record_dispatch_outcome(
            tenant.as_str(),
            entity_type,
            action,
            ask_outcome_for_metrics,
            outcome.attempts,
            outcome.elapsed,
        );

        // W2 / temper#146: on a successful first-ask against a freshly
        // spawned actor, record the cold-start duration.
        if let Some(cold_start_begin) = cold_start_outcome_start
            && outcome.result.is_ok()
        {
            crate::runtime_metrics::record_actor_cold_start_duration(
                entity_type,
                cold_start_begin.elapsed(),
            );
        }

        let response = match outcome.result {
            Ok(response) => response,
            Err(e) => {
                let entry = TrajectoryEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    action: action.to_string(),
                    success: false,
                    from_status: None,
                    to_status: None,
                    error: Some(format!("Actor dispatch failed: {e}")),
                    agent_id: agent_ctx.agent_id.clone(),
                    session_id: agent_ctx.session_id.clone(),
                    authz_denied: None,
                    denied_resource: None,
                    denied_module: None,
                    source: Some(TrajectorySource::Entity),
                    spec_governed: None,
                    agent_type: agent_ctx.agent_type.clone(),
                    request_body: Some(action_params.clone()),
                    intent: agent_ctx.intent.clone(),
                    matched_policy_ids: None,
                    capture_seq: None,
                };
                let request_body_str = truncate_request_body_for_log(&action_params.to_string());
                let from_status = entry.from_status.as_deref().unwrap_or("unknown");
                let to_status = entry.to_status.as_deref().unwrap_or("unknown");
                let observation_metadata =
                    agent_ctx.observation_metadata_json().unwrap_or_default();
                tracing::info!(
                    tenant = %entry.tenant,
                    entity_type = %entry.entity_type,
                    entity_id = %entry.entity_id,
                    action = %entry.action,
                    success = entry.success,
                    from_status = ?entry.from_status,
                    to_status = ?entry.to_status,
                    error = ?entry.error,
                    source = ?entry.source,
                    authz_denied = ?entry.authz_denied,
                    spec_governed = ?entry.spec_governed,
                    request_body = %request_body_str,
                    agent_id = entry.agent_id.as_deref().unwrap_or(""),
                    session_id = entry.session_id.as_deref().unwrap_or(""),
                    agent_type = entry.agent_type.as_deref().unwrap_or(""),
                    intent = entry.intent.as_deref().unwrap_or(""),
                    observation_metadata = %observation_metadata,
                    "app usage: {}.{} {} -> {} on {} failed",
                    entry.entity_type,
                    entry.action,
                    from_status,
                    to_status,
                    entry.entity_id
                );
                if !entry.success {
                    tracing::warn!(
                        tenant = %entry.tenant,
                        entity_type = %entry.entity_type,
                        entity_id = %entry.entity_id,
                        action = %entry.action,
                        error = ?entry.error,
                        authz_denied = ?entry.authz_denied,
                        source = ?entry.source,
                        "unmet_intent"
                    );
                }
                tracing::Span::current().record("success", false);
                if let Some(ref err) = entry.error {
                    tracing::Span::current().record("error_msg", err.as_str());
                }
                self.persist_trajectory_entry_background(entry);
                return Err(DispatchError::from_actor_error(e, outcome.attempts));
            }
        };

        // Run all post-dispatch effects through the dedicated pipeline.
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            dispatch_idempotency_key: idempotency_key.as_deref(),
            action_params: &action_params,
            await_integration,
        };
        let response = self.run_post_dispatch_effects(&ctx, response).await;
        if response.success
            && let Some(ref idem_key) = idempotency_key
        {
            let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
            self.idempotency_cache
                .mark_effects_applied(&actor_key, idem_key);
        }
        if response.success {
            self.clear_commons_storage_projection_cache_for_entity(entity_type);
        }

        tracing::Span::current().record("success", response.success);
        if let Some(ref err) = response.error {
            tracing::Span::current().record("error_msg", err.as_str());
        }
        if workflow_root_active && let Some(workflow_run_id) = agent_ctx.workflow_run_id.clone() {
            self.workflow_spans.finish_if_terminal_after_drain(
                workflow_run_id,
                entity_type.to_string(),
                entity_id.to_string(),
                response.state.status.clone(),
            );
        }

        Ok(response)
    }
}

#[cfg(test)]
mod log_truncation_tests {
    use super::{LOG_REQUEST_BODY_MAX_BYTES, truncate_request_body_for_log};

    #[test]
    fn short_body_is_logged_verbatim() {
        let body = r#"{"ProductId":"p-1"}"#;
        assert_eq!(truncate_request_body_for_log(body), body);
    }

    #[test]
    fn oversized_ascii_body_is_marked_truncated() {
        let body = "a".repeat(LOG_REQUEST_BODY_MAX_BYTES + 100);
        let truncated = truncate_request_body_for_log(&body);
        assert!(truncated.ends_with("[truncated]"));
        assert_eq!(
            truncated.len(),
            LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len()
        );
    }

    #[test]
    fn oversized_multibyte_body_does_not_panic_at_the_cap() {
        // 3-byte characters: the cap at 4096 falls mid-character (4096 % 3 != 0),
        // which a raw byte slice would panic on.
        let body = "\u{4e16}".repeat(LOG_REQUEST_BODY_MAX_BYTES);
        let truncated = truncate_request_body_for_log(&body);
        assert!(truncated.ends_with("[truncated]"));
        assert!(truncated.len() <= LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len());
    }
}
