//! Fenced persistence owner for one awaited collection-member integration.

use std::sync::Arc;
use std::{future::Future, time::Duration as StdDuration};

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use crate::storage::BoxedEventStore;
use crate::trigger::delivery::{
    AwaitedExecutionEvidenceV1, AwaitedExecutionFailureClass, AwaitedExecutionIdentityV1,
    AwaitedExecutionPhase, AwaitedOwnerFailureClass, ReactionDeliveryRecord,
    ReactionDeliveryStatus, append_delivery_record, delivery_record_append,
};

const EXECUTION_LEASE: Duration = Duration::seconds(30);
const EXECUTION_RENEWAL_PERIOD: StdDuration = StdDuration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub(crate) enum AwaitedOwnerError {
    DeadlineElapsed,
    FenceLost,
    StorageFailure,
}

impl std::fmt::Display for AwaitedOwnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DeadlineElapsed => "execution deadline elapsed",
            Self::FenceLost => "execution fence lost",
            Self::StorageFailure => "execution evidence storage failure",
        })
    }
}

struct OwnerState {
    record: ReactionDeliveryRecord,
    sequence: u64,
}

/// Durable callback replay material returned after binding an execution.
#[derive(Debug, Clone)]
pub(crate) struct AwaitedExecutionReplay {
    pub(crate) phase: AwaitedExecutionPhase,
    pub(crate) callback_action: Option<String>,
    pub(crate) callback_params: Option<serde_json::Value>,
    pub(crate) execution_failure: Option<AwaitedExecutionFailureClass>,
}

/// Serializes renewal and evidence appends for one in-process fenced owner.
pub(crate) struct AwaitedExecutionOwner {
    store: BoxedEventStore,
    fencing_token: u64,
    deadline: DateTime<Utc>,
    state: tokio::sync::Mutex<OwnerState>,
}

impl std::fmt::Debug for AwaitedExecutionOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwaitedExecutionOwner")
            .field("fencing_token", &self.fencing_token)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl AwaitedExecutionOwner {
    async fn append_fenced(
        &self,
        owner: &mut OwnerState,
        workflow_event_type: &str,
    ) -> Result<(), AwaitedOwnerError> {
        let delivery_append = delivery_record_append(owner.sequence, &owner.record)
            .map_err(|_| AwaitedOwnerError::StorageFailure)?;
        let mut appends = vec![delivery_append];
        if let Some(collection) = owner.record.intent.collection.as_ref() {
            appends.push(
                crate::trigger::collection_workflow::delivery_fence_append(
                    &self.store,
                    &owner.record.intent.tenant,
                    &owner.record.intent.delivery_id,
                    collection,
                    workflow_event_type,
                )
                .await
                .map_err(|error| match error {
                    crate::trigger::collection_workflow::DeliveryFenceError::FenceLost(reason) => {
                        tracing::debug!(%reason, "awaited collection delivery fence lost");
                        AwaitedOwnerError::FenceLost
                    }
                    crate::trigger::collection_workflow::DeliveryFenceError::Storage(reason) => {
                        tracing::error!(%reason, "awaited collection delivery fence read failed");
                        AwaitedOwnerError::StorageFailure
                    }
                })?,
            );
        }
        let results = self
            .store
            .append_batch(&appends)
            .await
            .map_err(|error| match error {
                temper_runtime::persistence::PersistenceError::ConcurrencyViolation { .. } => {
                    AwaitedOwnerError::FenceLost
                }
                _ => AwaitedOwnerError::StorageFailure,
            })?;
        owner.sequence = results[0].sequence_nr;
        Ok(())
    }

    pub(crate) fn new(
        store: BoxedEventStore,
        record: ReactionDeliveryRecord,
        sequence: u64,
        deadline: DateTime<Utc>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fencing_token: record.fencing_token,
            store,
            deadline,
            state: tokio::sync::Mutex::new(OwnerState { record, sequence }),
        })
    }

    pub(crate) async fn bind(
        &self,
        integration_name: &str,
        module_name: &str,
        module_digest: &str,
        success_callback: &str,
        failure_callback: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<AwaitedExecutionReplay, String> {
        let mut owner = self.state.lock().await;
        let execution_id = execution_id(
            &owner.record,
            integration_name,
            module_name,
            module_digest,
            success_callback,
            failure_callback,
        );
        let identity = AwaitedExecutionIdentityV1 {
            execution_id,
            integration_name: integration_name.to_string(),
            module_name: module_name.to_string(),
            module_digest: module_digest.to_string(),
            success_callback: success_callback.to_string(),
            failure_callback: failure_callback.map(str::to_string),
            schema_pin: owner.record.intent.schema_pin.clone(),
            deadline: self.deadline,
        };
        let prior_record = owner.record.clone();
        owner
            .record
            .bind_awaited_execution(self.fencing_token, identity, now)?;
        if let Err(error) = self
            .append_fenced(&mut owner, "CollectionWorkflow::AwaitedExecutionBoundV1")
            .await
        {
            owner.record = prior_record;
            return Err(error.to_string());
        }
        replay(
            owner
                .record
                .awaited_execution
                .as_ref()
                .expect("binding creates awaited evidence"),
        )
    }

    pub(crate) async fn renew(
        &self,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, AwaitedOwnerError> {
        let mut owner = self.state.lock().await;
        if now >= self.deadline {
            return Err(AwaitedOwnerError::DeadlineElapsed);
        }
        let prior_record = owner.record.clone();
        let expiry = if let Some(execution_id) = owner
            .record
            .awaited_execution
            .as_ref()
            .map(|evidence| evidence.identity.execution_id.clone())
        {
            owner
                .record
                .renew_awaited_execution(self.fencing_token, &execution_id, now, EXECUTION_LEASE)
                .map_err(|_| AwaitedOwnerError::FenceLost)?
        } else {
            if owner.record.status != ReactionDeliveryStatus::Dispatching
                || owner.record.fencing_token != self.fencing_token
                || now >= self.deadline
            {
                return Err(AwaitedOwnerError::FenceLost);
            }
            let expiry = (now + EXECUTION_LEASE).min(self.deadline);
            owner.record.lease_expires_at = Some(expiry);
            expiry
        };
        if let Err(error) = self
            .append_fenced(&mut owner, "CollectionWorkflow::AwaitedExecutionRenewedV1")
            .await
        {
            owner.record = prior_record;
            return Err(error);
        }
        crate::runtime_metrics::record_reaction_delivery_event(
            owner.record.intent.kind.metric_label(),
            "awaited_execution_lease_renewed",
        );
        Ok(expiry)
    }

    pub(crate) async fn record_owner_failure(
        &self,
        class: AwaitedOwnerFailureClass,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let mut owner = self.state.lock().await;
        owner
            .record
            .record_awaited_owner_failure(self.fencing_token, class, now)?;
        owner.sequence = append_delivery_record(&self.store, owner.sequence, &owner.record)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) async fn record_callback_failure(
        &self,
        class: AwaitedExecutionFailureClass,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let mut owner = self.state.lock().await;
        let prior_record = owner.record.clone();
        owner
            .record
            .record_awaited_callback_failure(self.fencing_token, class, now)?;
        if let Err(error) = self
            .append_fenced(&mut owner, "CollectionWorkflow::AwaitedCallbackFailedV1")
            .await
        {
            owner.record = prior_record;
            return Err(error.to_string());
        }
        Ok(())
    }

    pub(crate) async fn complete(
        &self,
        execution_id: &str,
        succeeded: bool,
        callback_action: Option<&str>,
        callback_params: Option<serde_json::Value>,
        failure_class: Option<AwaitedExecutionFailureClass>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let mut owner = self.state.lock().await;
        let prior_record = owner.record.clone();
        owner.record.record_awaited_completion(
            self.fencing_token,
            execution_id,
            succeeded,
            callback_action,
            callback_params,
            failure_class,
            now,
        )?;
        if let Err(error) = self
            .append_fenced(
                &mut owner,
                "CollectionWorkflow::AwaitedExecutionCompletedV1",
            )
            .await
        {
            owner.record = prior_record;
            return Err(error.to_string());
        }
        crate::runtime_metrics::record_reaction_delivery_event(
            owner.record.intent.kind.metric_label(),
            if succeeded {
                "awaited_execution_succeeded"
            } else {
                "awaited_execution_failed"
            },
        );
        Ok(())
    }

    pub(crate) async fn snapshot(&self) -> (ReactionDeliveryRecord, u64) {
        let owner = self.state.lock().await;
        (owner.record.clone(), owner.sequence)
    }
}

/// Drive one awaited dispatch while renewing its exact durable fence.
///
/// The wake-up clock is intentionally separate from the persisted decision
/// clock. Production uses Tokio only to wake the task and `sim_now` for every
/// durable timestamp; deterministic tests inject their logical clock here.
pub(crate) async fn run_with_renewal<T, F, N>(
    owner: &Arc<AwaitedExecutionOwner>,
    delivery_id: &str,
    delivery_kind: crate::trigger::delivery::DeliveryKind,
    dispatch: F,
    now: N,
) -> Result<T, String>
where
    F: Future<Output = T>,
    N: Fn() -> DateTime<Utc>,
{
    let mut dispatch = std::pin::pin!(dispatch);
    let mut renewal = tokio::time::interval(EXECUTION_RENEWAL_PERIOD);
    renewal.tick().await; // determinism-ok: consume immediate tick; injected clock drives decisions
    loop {
        tokio::select! { // determinism-ok: bounded wake-up driver; durable order is fenced
            result = &mut dispatch => return Ok(result),
            _ = renewal.tick() => {
                let observed_at = now();
                if let Err(error) = owner.renew(observed_at).await {
                    let failure = match error {
                        AwaitedOwnerError::DeadlineElapsed => {
                            AwaitedOwnerFailureClass::DeadlineElapsed
                        }
                        AwaitedOwnerError::StorageFailure => {
                            AwaitedOwnerFailureClass::StorageFailure
                        }
                        AwaitedOwnerError::FenceLost => {
                            AwaitedOwnerFailureClass::RenewalLost
                        }
                    };
                    if let Err(evidence_error) = owner
                        .record_owner_failure(failure, observed_at)
                        .await
                    {
                        crate::runtime_metrics::record_reaction_delivery_event(
                            delivery_kind.metric_label(),
                            "awaited_owner_failure_evidence_write_failed",
                        );
                        tracing::error!(
                            delivery_id,
                            fencing_token = owner.fencing_token,
                            error = %evidence_error,
                            "failed to persist awaited owner failure evidence"
                        );
                    }
                    crate::runtime_metrics::record_reaction_delivery_event(
                        delivery_kind.metric_label(),
                        "awaited_execution_renewal_failed",
                    );
                    tracing::warn!(
                        delivery_id,
                        fencing_token = owner.fencing_token,
                        failure = ?failure,
                        "awaited execution lease renewal failed closed"
                    );
                    return Err(format!("AwaitedExecutionRenewalLost: {error}"));
                }
            }
        }
    }
}

fn replay(evidence: &AwaitedExecutionEvidenceV1) -> Result<AwaitedExecutionReplay, String> {
    if let Some(params) = evidence.callback_params.as_ref() {
        let bytes = serde_json::to_vec(params)
            .map_err(|error| format!("awaited callback evidence is invalid: {error}"))?;
        let mut digest = Sha256::new();
        digest.update(b"temper-awaited-callback-v1\0");
        digest.update(bytes);
        let actual = format!("sha256:{:x}", digest.finalize());
        if evidence.callback_digest.as_deref() != Some(actual.as_str()) {
            return Err("awaited callback evidence digest mismatch".to_string());
        }
    } else if evidence.callback_digest.is_some() {
        return Err("awaited callback digest has no matching parameters".to_string());
    }
    Ok(AwaitedExecutionReplay {
        phase: evidence.phase,
        callback_action: evidence.callback_action.clone(),
        callback_params: evidence.callback_params.clone(),
        execution_failure: evidence.execution_failure,
    })
}

fn execution_id(
    record: &ReactionDeliveryRecord,
    integration_name: &str,
    module_name: &str,
    module_digest: &str,
    success_callback: &str,
    failure_callback: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"temper-awaited-execution-v1\0");
    for component in [
        record.intent.tenant.as_str(),
        record.intent.delivery_id.as_str(),
        integration_name,
        module_name,
        module_digest,
        success_callback,
        failure_callback.unwrap_or(""),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    if let Some(collection) = record.intent.collection.as_ref() {
        for component in [
            collection.workflow_id.as_str(),
            collection.member_id.as_deref().unwrap_or(""),
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
    }
    if let Some(pin) = record.intent.schema_pin.as_ref() {
        for component in [
            pin.execution.bundle_digest.as_str(),
            pin.action_digest.as_str(),
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
    }
    format!("awaited-execution:{:x}", digest.finalize())
}
