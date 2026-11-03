//! Prepare runtime-generated inputs without exempting ordinary action requests.
use super::owed_writes::{Attempt, OwedKind, OwedMove, OwedTry};
use super::{DispatchCommand, DispatchError, WasmDispatchMode, WasmEntityRef};
use crate::{entity_actor::EntityResponse, state::ServerState};
use serde_json::Value;
use temper_runtime::tenant::TenantId;

impl ServerState {
    /// A spawn whose child was not created. The refusal goes to the parent's
    /// observe stream, as before, and into its journal as `SpawnRefused`,
    /// which clears the field only while this request owns its reservation. The
    /// field keeps the id if the child exists anyway or could not be looked
    /// up.
    pub(super) async fn refuse_spawn(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        request: &crate::entity_actor::effects::SpawnRequest,
        action: &str,
        error: &str,
    ) {
        self.record_generated_callback_refusal(
            super::WasmEntityRef {
                tenant,
                entity_type: parent_type,
                entity_id: parent_id,
            },
            action,
            error,
        );
        use crate::entity_actor::spawn_refusal::{ChildLookup, SpawnRefusal};
        let child = match self
            .entity_exists_durably(tenant, &request.entity_type, &request.entity_id)
            .await
        {
            Ok(true) => ChildLookup::Exists,
            Ok(false) => ChildLookup::Absent,
            Err(lookup_error) => {
                tracing::warn!(
                    tenant = %tenant,
                    parent_type,
                    parent_id,
                    child_type = %request.entity_type,
                    child_id = %request.entity_id,
                    lookup_error = %lookup_error,
                    "refused child could not be looked up; the parent keeps its id"
                );
                ChildLookup::Unreadable
            }
        };
        // Explore the gap between the child lookup and the parent receiving
        // its refusal: another spawn may reserve the same id in between.
        temper_runtime::buggify::buggify_yield(32).await;
        let refusal = SpawnRefusal {
            reservation_id: request.reservation_id.clone(),
            child_type: request.entity_type.clone(),
            child_id: request.entity_id.clone(),
            field: request.store_id_in.clone(),
            child,
            error: error.to_owned(),
        };
        // Logged once, not retried: the parent was deleted, its event budget
        // is spent or the store is down, and the observe event above already
        // holds the refusal. This runs after the parent's transition commits,
        // so a refusal lost to a crash in that window is not replayed after
        // restart. Durable recovery of post-commit work belongs with durable
        // scheduled actions (#104), not a second journal here.
        if let Err(record_error) = self
            .record_spawn_refusal(tenant, parent_type, parent_id, refusal)
            .await
        {
            tracing::error!(
                tenant = %tenant,
                parent_type,
                parent_id,
                child_type = %request.entity_type,
                child_id = %request.entity_id,
                error,
                record_error = %record_error,
                "spawn refusal could not be recorded on the parent"
            );
        }
    }

    /// Project generated data onto a strict action's declared inputs. Constraint
    /// checks remain at the actor boundary, against its unmodified live state.
    pub(super) fn prepare_generated_action_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        mut params: Value,
    ) -> Result<Value, String> {
        let table = self
            .transition_table_for_dispatch(tenant, entity_type)
            .map_err(|error| error.to_string())?;
        if !table.strict_action_params {
            return Ok(params);
        }
        let contract = table
            .action_contracts
            .get(action)
            .ok_or_else(|| format!("Action '{action}' has no declared parameter contract"))?;
        let fields = params
            .as_object_mut()
            .ok_or_else(|| "Generated action parameters must be a JSON object".to_string())?;
        fields.retain(|name, _| contract.params.contains(name));
        Ok(params)
    }

    pub(super) fn record_generated_callback_refusal(
        &self,
        entity: WasmEntityRef<'_>,
        action: &str,
        error: &str,
    ) {
        self.record_answer_refusal(entity, action, error, &Value::Null);
    }

    /// A refused answer. The entity's observe stream gets the values it
    /// carried; the log gets only their names, since an answer's values can
    /// be anything an integration returned.
    pub(super) fn record_answer_refusal(
        &self,
        entity: WasmEntityRef<'_>,
        action: &str,
        error: &str,
        params: &Value,
    ) {
        let seq = self.next_entity_event_sequence(
            entity.tenant.as_str(),
            entity.entity_type,
            entity.entity_id,
        );
        self.record_entity_observe_event_with_seq(
            entity.tenant.as_str(),
            entity.entity_type,
            entity.entity_id,
            seq,
            "integration_callback_rejected",
            serde_json::json!({
                "action":action, "error":error, "params":params,
            }),
        );
        let param_names: Vec<&str> = params
            .as_object()
            .map(|fields| fields.keys().map(String::as_str).collect())
            .unwrap_or_default();
        tracing::warn!(tenant=%entity.tenant, entity_type=entity.entity_type,
            entity_id=entity.entity_id, action, error, ?param_names, "integration callback rejected");
    }

    /// Send an answer of a `source` integration ("WASM" or "adapter"): `sent`
    /// is its action and values, as the integration's caller under the
    /// answer's own key. Inline it runs as the caller and through its own
    /// integrations, and the caller gets its reply. In the background it also
    /// runs as the caller, and a dispatch error that is not the store's is returned
    /// for the integration's failure path (ADR-0152). Either way it is
    /// settled ([`Self::settle_answer`]): a write the store failed is owed.
    pub(super) async fn send_answer(
        &self,
        mut sent: OwedMove,
        mode: WasmDispatchMode,
        source: &str,
    ) -> Result<Option<EntityResponse>, String> {
        sent.agent_ctx = sent.agent_ctx.for_answer();
        match mode {
            WasmDispatchMode::Inline => {
                // Preserve inline semantics through nested WASM callbacks.
                // A public action may dispatch a validation callback that has
                // its own WASM trigger; returning before that nested trigger
                // commits lets concurrent requests observe stale detailed
                // fields while counters advance.
                let reply = super::wasm::dispatch_callback_action_boxed(
                    self,
                    DispatchCommand {
                        tenant: &sent.tenant,
                        entity_type: &sent.entity_type,
                        entity_id: &sent.entity_id,
                        action: &sent.action,
                        params: sent.params.clone(),
                        agent_ctx: &sent.agent_ctx,
                        await_integration: true,
                        await_reactions: true,
                    },
                )
                .await;
                self.settle_answer(sent, &reply);
                reply.map(Some).map_err(|error| error.to_string())
            }
            WasmDispatchMode::Background => {
                let reply = sent.send(self).await;
                let action = sent.action.clone();
                match (self.settle_answer(sent, &reply), reply) {
                    (Attempt::Failed(_), Err(error)) => {
                        let msg =
                            format!("failed to dispatch {source} callback '{action}': {error}");
                        tracing::error!(callback = %action, %error, "{msg}");
                        Err(msg)
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    /// Settle the first try of an answer, a module's or an adapter's, sent as
    /// `sent`: a reply that failed is journaled as a rejected answer, and a
    /// write the store failed is owed, sent again until it lands or fails
    /// otherwise (see `owed_writes`).
    fn settle_answer(
        &self,
        sent: OwedMove,
        reply: &Result<EntityResponse, DispatchError>,
    ) -> Attempt {
        let attempt = Attempt::of(reply);
        match &attempt {
            Attempt::Landed => {}
            // A dispatch error the caller reports itself, as before.
            Attempt::Failed(_) if reply.is_err() => {}
            Attempt::Failed(error) => {
                self.record_answer_refusal(sent.entity_ref(), &sent.action, error, &sent.params);
            }
            Attempt::StoreFailed(error) => {
                let owed = sent.owed(OwedKind::Answer);
                let sent = std::sync::Arc::new(sent);
                self.keep_owed_write(owed, error, move |state| {
                    answer_again(state, std::sync::Arc::clone(&sent))
                });
            }
        }
        attempt
    }

    /// A rejected callback is an observed refusal, not a new execution failure.
    /// Returning an integration error here would compensate potentially newer state.
    pub(super) async fn reject_generated_callback(
        &self,
        entity: WasmEntityRef<'_>,
        action: &str,
        error: String,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        self.record_generated_callback_refusal(entity, action, &error);
        match mode {
            WasmDispatchMode::Background => Ok(None),
            WasmDispatchMode::Inline => {
                let mut response = self
                    .get_tenant_entity_state(entity.tenant, entity.entity_type, entity.entity_id)
                    .await?;
                response.success = false;
                response.error = Some(error);
                Ok(Some(response))
            }
        }
    }
}

/// Send an owed answer again, journaling it if it fails otherwise this time:
/// its call has then ended, and its deadline row goes.
fn answer_again(state: ServerState, sent: std::sync::Arc<OwedMove>) -> OwedTry {
    Box::pin(async move {
        let attempt = Attempt::of(&sent.send(&state).await);
        if let Attempt::Failed(error) = &attempt {
            state.record_answer_refusal(sent.entity_ref(), &sent.action, error, &sent.params);
            if let Some(call) = &sent.agent_ctx.ends_call {
                state.end_call_row(call).await;
            }
        }
        attempt
    })
}
