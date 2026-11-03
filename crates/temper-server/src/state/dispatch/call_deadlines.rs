//! The server's side of a background call's deadline row
//! (`request_context::calls`): which calls a move may start, the row each
//! call answers or ends, and the answer a lost call's row fires.

use std::collections::BTreeSet;
use std::time::Duration;

use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Integration;
use temper_wasm::WasmResourceLimits;

use super::WasmDispatchMode;
use super::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};
use crate::entity_actor::EntityState;
use crate::request_context::calls::{call_arm_id, called_integration};
use crate::request_context::{AgentContext, BackgroundCall, ScheduledFire};
use crate::state::ServerState;

/// The `timeout_secs` an integration's config sets, if it sets one.
pub(super) fn configured_timeout(integration: &Integration) -> Option<Duration> {
    integration
        .config
        .get("timeout_secs")
        .and_then(|secs| secs.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// How long a call to `integration` may run: its `timeout_secs`, else the
/// engine's default limit. Dispatch hands the same number to the engine as
/// `max_duration`, and the engine stops the module there (temper-wasm
/// `engine/mod.rs` sets the store's epoch deadline and the deadline of every
/// host call it makes from it), so no call's module runs longer. No margin is added:
/// the row is armed again only after a restart, when the call is gone.
fn call_deadline(integration: &Integration) -> Duration {
    configured_timeout(integration).unwrap_or_else(|| WasmResourceLimits::default().max_duration)
}

impl ServerState {
    /// The background calls with an `on_failure` that a move on an
    /// `entity_type` record may start: for each trigger, the first WASM
    /// integration it names, as dispatch picks it
    /// (`dispatch_wasm_integrations_internal`).
    pub(super) fn background_calls(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<BackgroundCall> {
        let Ok(registry) = self.registry.read() else {
            return Vec::new();
        };
        let Some(spec) = registry.get_spec(tenant, entity_type) else {
            return Vec::new();
        };
        let mut triggers = BTreeSet::new();
        let mut calls = Vec::new();
        for integration in spec
            .integrations
            .iter()
            .filter(|integration| integration.integration_type == "wasm")
        {
            if !triggers.insert(integration.trigger.as_str()) {
                continue;
            }
            if let Some(on_failure) = &integration.on_failure {
                calls.push(BackgroundCall {
                    process_id: self.call_process_id,
                    trigger: integration.trigger.clone(),
                    integration: integration.name.clone(),
                    on_failure: on_failure.clone(),
                    deadline: call_deadline(integration),
                });
            }
        }
        calls
    }

    /// The deadline row of the call to `integration` that the move leaving
    /// `entity_state` started, when the move wrote one: a background call
    /// that names an `on_failure`. The move's event is the record's newest.
    pub(super) fn call_deadline_row(
        &self,
        tenant: &TenantId,
        entity_state: &EntityState,
        integration: &Integration,
        mode: WasmDispatchMode,
    ) -> Option<ScheduledFire> {
        if !matches!(mode, WasmDispatchMode::Background) || integration.on_failure.is_none() {
            return None;
        }
        Some(ScheduledFire {
            tenant: tenant.as_str().to_string(),
            entity_type: entity_state.entity_type.clone(),
            entity_id: entity_state.entity_id.clone(),
            arm_id: call_arm_id(entity_state.sequence_nr, &integration.name),
        })
    }

    /// Remove the deadline row of a call that has ended, unless its answer
    /// is owed: an owed answer removes the row when it lands, and a restart
    /// that loses it leaves the row to answer the call. `caller` started the
    /// call. An answer that landed already removed the row; removing it
    /// again does nothing.
    pub(super) fn end_call(&self, call: &ScheduledFire, integration: &str, caller: &AgentContext) {
        if caller
            .callback_key(integration)
            .is_some_and(|key| self.holds_owed_write(key.as_str()))
        {
            return;
        }
        let server = self.clone();
        let call = call.clone();
        // Settlement remains pending independently of the move's other calls.
        tokio::spawn(async move { server.end_call_row(&call).await }); // determinism-ok: background call settlement
    }

    /// Remove the deadline row of a call whose answer did not land and will
    /// not be sent again. Cancellation is durable before deletion; each
    /// failed write remains pending until it lands.
    pub(super) async fn end_call_row(&self, call: &ScheduledFire) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        let mut wait = FIRST_RETRY_WAIT;
        // First persist finality. A crash after this point, even with failed
        // deletion, leaves a cancelled row that recovery cannot fire.
        loop {
            match store
                .cancel_scheduled_action(
                    &call.tenant,
                    &call.entity_type,
                    &call.entity_id,
                    &call.arm_id,
                )
                .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(?call, %error, "call cancellation pending; retrying");
                    tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                    wait = (wait * 2).min(LAST_RETRY_WAIT);
                }
            }
        }
        wait = FIRST_RETRY_WAIT;
        loop {
            match store
                .delete_scheduled_action(
                    &call.tenant,
                    &call.entity_type,
                    &call.entity_id,
                    &call.arm_id,
                )
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(?call, %error, "cancelled call row cleanup pending; retrying");
                    tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                    wait = (wait * 2).min(LAST_RETRY_WAIT);
                }
            }
        }
    }

    /// The parameters the fire of the row `arm_id` sends as `action` on an
    /// `entity_type` record: a call's deadline answers the lost call as a
    /// failed call is answered; a schedule sends none.
    pub(super) fn scheduled_fire_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        arm_id: &str,
    ) -> serde_json::Value {
        match called_integration(arm_id) {
            Some(integration) => self.lost_call_params(tenant, entity_type, action, integration),
            None => serde_json::json!({}),
        }
    }

    /// The parameters a lost call's deadline answers it with, as a failed
    /// call's are (`handle_wasm_failure`), projected onto `action`'s
    /// declared inputs when its type is strict.
    fn lost_call_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        integration: &str,
    ) -> serde_json::Value {
        let error = format!(
            "integration '{integration}' did not answer by its deadline: \
             the server restarted while the call was in flight"
        );
        let params = serde_json::json!({
            "error": error,
            "error_message": error,
            "integration": integration,
        });
        // A strict action without a contract refuses the parameters at its
        // actor, as it refuses a failed call's answer.
        self.prepare_generated_action_params(tenant, entity_type, action, params.clone())
            .unwrap_or(params)
    }
}
