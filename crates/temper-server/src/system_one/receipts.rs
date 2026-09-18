//! Durable first-writer-wins evaluation records, separate from entity state.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_spec::automaton::SystemOneGuard;

use super::evidence::json_digest;
use super::provider::SystemOneProvider;
use crate::storage::BoxedEventStore;

/// Immutable context binding one provider response to an attempted action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReceiptBinding {
    pub tenant: String,
    pub principal: String,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub attempt_id: String,
    pub expected_state: String,
    pub expected_table: String,
    pub params_digest: String,
    pub guard_key: String,
    pub guard_slot: usize,
}

/// A validated answer, negative assertion, or provider error saved for retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluationReceipt {
    pub binding: ReceiptBinding,
    pub request: Value,
    pub response: Option<Value>,
    pub passed: bool,
    pub error: Option<String>,
}

fn persistence_id(binding: &ReceiptBinding) -> Result<String, String> {
    // Deliberately exclude input/spec/state from the stream identity: changing
    // these while reusing an attempt is a conflict, not another model sample.
    let identity = serde_json::json!({
        "tenant": binding.tenant, "principal": binding.principal,
        "entity_type": binding.entity_type, "entity_id": binding.entity_id,
        "action": binding.action, "attempt": binding.attempt_id,
        "guard_slot": binding.guard_slot,
    });
    Ok(format!(
        "{}:__SystemOneEvaluation:{}",
        binding.tenant,
        json_digest(&identity)?
    ))
}

async fn read_receipt(
    store: &BoxedEventStore,
    id: &str,
) -> Result<Option<EvaluationReceipt>, String> {
    let events = store
        .read_events(id, 0)
        .await
        .map_err(|_| "cannot read system_one evaluation receipt".to_string())?;
    if events.is_empty() {
        return Ok(None);
    }
    if events.len() != 1
        || events[0].event_type != "SystemOneEvaluation"
        || events[0].sequence_nr != 1
    {
        return Err("invalid system_one evaluation journal".into());
    }
    serde_json::from_value(events[0].payload.clone())
        .map(Some)
        .map_err(|_| "invalid system_one evaluation receipt".into())
}

/// Recover an existing answer using its immutable binding and captured request.
/// Resolved blob contents need not remain available after their receipt is durable.
pub(crate) async fn recorded_receipt(
    store: &BoxedEventStore,
    guard: &SystemOneGuard,
    binding: &ReceiptBinding,
) -> Result<Option<(String, EvaluationReceipt)>, String> {
    let id = persistence_id(binding)?;
    let Some(receipt) = read_receipt(store, &id).await? else {
        return Ok(None);
    };
    check_receipt(&receipt, binding, &receipt.request, guard)?;
    Ok(Some((id, receipt)))
}

/// Resolve an evaluation through the real provider/store boundary.
///
/// A conflicting append reloads the first durable answer. Neither failed
/// assertions nor provider failures are resampled for the same logical attempt.
pub(crate) async fn resolve_receipt(
    store: &BoxedEventStore,
    provider: &dyn SystemOneProvider,
    guard: &SystemOneGuard,
    binding: ReceiptBinding,
    request: Value,
) -> Result<(String, EvaluationReceipt), String> {
    let id = persistence_id(&binding)?;
    if let Some(receipt) = read_receipt(store, &id).await? {
        check_receipt(&receipt, &binding, &request, guard)?;
        return Ok((id, receipt));
    }
    let result = provider.evaluate(&binding.tenant, &request).await;
    let (response, passed, error) = match result {
        Ok(response) => match guard.evaluate_response(&response) {
            Ok(passed) => (Some(response), passed, None),
            Err(_) => (
                None,
                false,
                Some("invalid system_one provider response".into()),
            ),
        },
        Err(error) => (None, false, Some(error)),
    };
    let receipt = EvaluationReceipt {
        binding: binding.clone(),
        request: request.clone(),
        response,
        passed,
        error,
    };
    let envelope = PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "SystemOneEvaluation".into(),
        payload: serde_json::to_value(&receipt)
            .map_err(|_| "cannot serialize system_one receipt")?,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: id.clone(),
        },
    };
    match store.append(&id, 0, &[envelope]).await {
        Ok(1) => Ok((id, receipt)),
        Ok(_) => Err("invalid system_one receipt append sequence".into()),
        Err(PersistenceError::ConcurrencyViolation { .. }) => {
            let winner = read_receipt(store, &id)
                .await?
                .ok_or("system_one receipt conflict without durable answer")?;
            check_receipt(&winner, &binding, &request, guard)?;
            Ok((id, winner))
        }
        Err(_) => Err("cannot persist system_one evaluation receipt".into()),
    }
}

fn check_receipt(
    receipt: &EvaluationReceipt,
    binding: &ReceiptBinding,
    request: &Value,
    guard: &SystemOneGuard,
) -> Result<(), String> {
    if &receipt.binding != binding || &receipt.request != request {
        return Err("system_one action attempt was reused with different or stale inputs".into());
    }
    if let Some(response) = &receipt.response {
        if receipt.error.is_some() || guard.evaluate_response(response)? != receipt.passed {
            return Err("inconsistent system_one evaluation receipt".into());
        }
    } else if receipt.passed || receipt.error.is_none() {
        return Err("incomplete system_one evaluation receipt".into());
    }
    Ok(())
}
