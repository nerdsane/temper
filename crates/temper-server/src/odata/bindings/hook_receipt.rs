use super::*;

const BOUND_ACTION_HOOK_RECEIPT_EVENT_TYPE: &str = "Temper.Internal.BoundActionHookReceipt.v1";
const BOUND_ACTION_HOOK_RECEIPT_SCHEMA: &str = "temper.bound-action-hook-receipt.v1";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct BoundActionHookReceipt {
    schema: String,
    tenant: String,
    entity_type: String,
    entity_id: String,
    action: String,
    durable_idempotency_key: String,
    request_fingerprint: String,
    completed: bool,
    hook_output: Option<serde_json::Value>,
}

impl BoundActionHookReceipt {
    #[expect(
        clippy::too_many_arguments,
        reason = "the durable receipt constructor binds every exact governed request component"
    )]
    pub(super) fn new(
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        durable_idempotency_key: String,
        request_fingerprint: String,
        completed: bool,
        hook_output: Option<serde_json::Value>,
    ) -> Self {
        Self {
            schema: BOUND_ACTION_HOOK_RECEIPT_SCHEMA.to_string(),
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: action.to_string(),
            durable_idempotency_key,
            request_fingerprint,
            completed,
            hook_output,
        }
    }

    pub(super) fn hook_output(&self) -> Option<&serde_json::Value> {
        self.hook_output.as_ref()
    }

    pub(super) fn is_completed(&self) -> bool {
        self.completed
    }

    fn same_claim(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.tenant == other.tenant
            && self.entity_type == other.entity_type
            && self.entity_id == other.entity_id
            && self.action == other.action
            && self.durable_idempotency_key == other.durable_idempotency_key
            && self.request_fingerprint == other.request_fingerprint
    }

    fn persistence_id(&self) -> String {
        let digest = ServerState::spec_publication_intent(
            "bound-action-hook-receipt-id",
            [
                ("entity_type", self.entity_type.as_bytes()),
                ("entity_id", self.entity_id.as_bytes()),
                (
                    "durable_idempotency_key",
                    self.durable_idempotency_key.as_bytes(),
                ),
            ],
        );
        format!("{}:_BoundActionHook:{digest}", self.tenant)
    }
}

async fn load_receipt(
    state: &ServerState,
    intended: &BoundActionHookReceipt,
) -> Result<Option<BoundActionHookReceipt>, String> {
    let Some((store, _backend)) = state.event_journal() else {
        return Ok(None);
    };
    let persistence_id = intended.persistence_id();
    let events = store
        .read_events(&persistence_id, 0)
        .await
        .map_err(|error| format!("failed to read bound-action hook receipt: {error}"))?;
    let mut latest = None;
    let mut completed = None;
    for event in events {
        if event.event_type != BOUND_ACTION_HOOK_RECEIPT_EVENT_TYPE {
            continue;
        }
        let receipt =
            serde_json::from_value::<BoundActionHookReceipt>(event.payload).map_err(|error| {
                format!(
                    "malformed bound-action hook receipt at sequence {}: {error}",
                    event.sequence_nr
                )
            })?;
        if !receipt.same_claim(intended) {
            return Err(
                "durable bound-action hook receipt conflicts with this exact request".to_string(),
            );
        }
        if receipt.completed {
            if let Some(existing) = completed.as_ref()
                && existing != &receipt
            {
                return Err("durable bound-action hook has conflicting completions".to_string());
            }
            completed = Some(receipt.clone());
        } else if completed.is_some() {
            return Err("durable bound-action hook receipt regressed after completion".to_string());
        }
        latest = Some(receipt);
    }
    let Some(receipt) = completed.or(latest) else {
        return Ok(None);
    };
    if intended.completed && receipt.completed && receipt.hook_output != intended.hook_output {
        return Err("durable bound-action hook has conflicting output".to_string());
    }
    Ok(Some(receipt))
}

pub(super) async fn load_bound_action_hook_receipt(
    state: &ServerState,
    intended: &BoundActionHookReceipt,
) -> Result<Option<BoundActionHookReceipt>, String> {
    load_receipt(state, intended).await
}

pub(super) async fn persist_bound_action_hook_receipt(
    state: &ServerState,
    intended: &BoundActionHookReceipt,
) -> Result<(), String> {
    let Some((store, _backend)) = state.event_journal() else {
        return Ok(());
    };
    if let Some(existing) = load_receipt(state, intended).await?
        && (!intended.completed || existing.completed)
    {
        return Ok(());
    }

    let persistence_id = intended.persistence_id();
    let boundary = store
        .journal_boundary(&persistence_id)
        .await
        .map_err(|error| {
            format!("failed to capture bound-action hook receipt boundary: {error}")
        })?;
    let payload = serde_json::to_value(intended)
        .map_err(|error| format!("failed to encode bound-action hook receipt: {error}"))?;
    let envelope = temper_runtime::persistence::PersistenceEnvelope {
        sequence_nr: 0,
        event_type: BOUND_ACTION_HOOK_RECEIPT_EVENT_TYPE.to_string(),
        payload,
        metadata: temper_runtime::persistence::EventMetadata {
            event_id: temper_runtime::scheduler::sim_uuid(),
            causation_id: temper_runtime::scheduler::sim_uuid(),
            correlation_id: temper_runtime::scheduler::sim_uuid(),
            timestamp: temper_runtime::scheduler::sim_now(),
            actor_id: persistence_id.clone(),
        },
    };
    if let Err(error) = store
        .append(&persistence_id, boundary.latest_sequence, &[envelope])
        .await
    {
        let recovered = load_receipt(state, intended).await?;
        let committed = recovered.is_some_and(|receipt| {
            receipt.completed == intended.completed
                && (!intended.completed || receipt.hook_output == intended.hook_output)
        });
        if committed {
            return Ok(());
        }
        return Err(format!(
            "failed to persist bound-action hook receipt: {error}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
    use temper_runtime::scheduler::{sim_now, sim_uuid};
    use temper_store_sim::SimEventStore;

    use super::*;
    use crate::registry::SpecRegistry;
    use crate::storage::StorageStack;

    #[tokio::test]
    async fn malformed_reserved_hook_receipt_fails_closed() {
        let store = SimEventStore::no_faults(4_011);
        let mut state = ServerState::from_registry(
            ActorSystem::new("malformed-hook-receipt"),
            SpecRegistry::new(),
        );
        state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
        let tenant = TenantId::default();
        let intended = BoundActionHookReceipt::new(
            &tenant,
            "Order",
            "order-1",
            "Cancel",
            "durable-key".to_string(),
            "request-fingerprint".to_string(),
            false,
            None,
        );
        let persistence_id = intended.persistence_id();
        store
            .append(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: BOUND_ACTION_HOOK_RECEIPT_EVENT_TYPE.to_string(),
                    payload: serde_json::json!({"schema": "malformed-reserved-receipt"}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                }],
            )
            .await
            .expect("seed malformed reserved receipt");

        let error = load_bound_action_hook_receipt(&state, &intended)
            .await
            .expect_err("reserved receipt corruption must block hook replay");
        assert!(error.contains("malformed bound-action hook receipt"));
    }
}
