//! Awaited execution lifecycle mutations.

use sha2::{Digest, Sha256};

use super::*;

impl ReactionDeliveryRecord {
    pub(crate) fn bind_awaited_execution(
        &mut self,
        fencing_token: u64,
        identity: AwaitedExecutionIdentityV1,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        if now >= identity.deadline {
            return Err("awaited execution deadline has elapsed".to_string());
        }
        if let Some(evidence) = self.awaited_execution.as_mut() {
            if evidence.identity != identity {
                return Err("awaited execution identity mismatch".to_string());
            }
            evidence.fencing_token = fencing_token;
            if evidence.phase == AwaitedExecutionPhase::Executing {
                evidence.started_at = now;
            }
            return Ok(());
        }
        self.awaited_execution = Some(AwaitedExecutionEvidenceV1 {
            identity,
            phase: AwaitedExecutionPhase::Executing,
            fencing_token,
            started_at: now,
            completed_at: None,
            callback_action: None,
            callback_params: None,
            callback_digest: None,
            callback_accepted_at: None,
            callback_sequence: None,
            execution_failure: None,
            callback_failure: None,
        });
        Ok(())
    }

    pub(crate) fn renew_awaited_execution(
        &mut self,
        fencing_token: u64,
        execution_id: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<DateTime<Utc>, String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        if lease <= Duration::zero() {
            return Err("delivery lease must be positive".to_string());
        }
        if self.lease_expires_at.is_none_or(|expiry| now >= expiry) {
            return Err("awaited execution lease has expired".to_string());
        }
        let evidence = self
            .awaited_execution
            .as_ref()
            .ok_or_else(|| "awaited execution is not bound".to_string())?;
        if evidence.fencing_token != fencing_token || evidence.identity.execution_id != execution_id
        {
            return Err("stale awaited execution owner".to_string());
        }
        if evidence.phase == AwaitedExecutionPhase::CallbackAccepted {
            return Err("awaited execution callback is already accepted".to_string());
        }
        if now >= evidence.identity.deadline {
            return Err("awaited execution deadline has elapsed".to_string());
        }
        let expiry = (now + lease).min(evidence.identity.deadline);
        self.lease_expires_at = Some(expiry);
        Ok(expiry)
    }

    pub(crate) fn record_awaited_owner_failure(
        &mut self,
        fencing_token: u64,
        class: AwaitedOwnerFailureClass,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        self.awaited_owner_failure = Some(AwaitedOwnerFailureEvidenceV1 {
            class,
            fencing_token,
            occurred_at: now,
        });
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "durable execution identity is explicit"
    )]
    pub(crate) fn record_awaited_completion(
        &mut self,
        fencing_token: u64,
        execution_id: &str,
        succeeded: bool,
        callback_action: Option<&str>,
        callback_params: Option<serde_json::Value>,
        failure_class: Option<AwaitedExecutionFailureClass>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        if self.lease_expires_at.is_none_or(|expiry| now >= expiry) {
            return Err("awaited execution lease has expired".to_string());
        }
        let encoded = callback_params
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| format!("awaited callback evidence is invalid: {error}"))?;
        if encoded
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_AWAITED_CALLBACK_EVIDENCE_BYTES)
        {
            return Err("CompletionEvidenceBudgetExceeded".to_string());
        }
        let evidence = self
            .awaited_execution
            .as_mut()
            .ok_or_else(|| "awaited execution is not bound".to_string())?;
        if evidence.fencing_token != fencing_token || evidence.identity.execution_id != execution_id
        {
            return Err("stale awaited execution owner".to_string());
        }
        if evidence.phase != AwaitedExecutionPhase::Executing {
            return Err("awaited execution is not executing".to_string());
        }
        if now > evidence.identity.deadline {
            return Err("awaited execution deadline has elapsed".to_string());
        }
        evidence.phase = if succeeded {
            AwaitedExecutionPhase::ExecutionSucceeded
        } else {
            AwaitedExecutionPhase::ExecutionFailed
        };
        evidence.completed_at = Some(now);
        evidence.callback_action = callback_action.map(str::to_string);
        evidence.callback_digest = encoded.map(|bytes| {
            let mut digest = Sha256::new();
            digest.update(b"temper-awaited-callback-v1\0");
            digest.update(bytes);
            format!("sha256:{:x}", digest.finalize())
        });
        evidence.callback_params = callback_params;
        evidence.execution_failure = failure_class;
        Ok(())
    }

    pub(crate) fn accept_awaited_callback(
        &mut self,
        fencing_token: u64,
        execution_id: &str,
        callback_action: &str,
        callback_sequence: u64,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        if self.lease_expires_at.is_none_or(|expiry| now >= expiry) {
            return Err("awaited execution lease has expired".to_string());
        }
        let evidence = self
            .awaited_execution
            .as_mut()
            .ok_or_else(|| "awaited execution is not bound".to_string())?;
        if evidence.fencing_token != fencing_token || evidence.identity.execution_id != execution_id
        {
            return Err("stale awaited execution owner".to_string());
        }
        if !matches!(
            evidence.phase,
            AwaitedExecutionPhase::ExecutionSucceeded | AwaitedExecutionPhase::ExecutionFailed
        ) || evidence.callback_action.as_deref() != Some(callback_action)
        {
            return Err("awaited callback does not match completion evidence".to_string());
        }
        if now > evidence.identity.deadline {
            return Err("awaited execution deadline has elapsed".to_string());
        }
        evidence.phase = AwaitedExecutionPhase::CallbackAccepted;
        evidence.callback_failure = None;
        evidence.callback_accepted_at = Some(now);
        evidence.callback_sequence = Some(callback_sequence);
        Ok(())
    }

    pub(crate) fn record_awaited_callback_failure(
        &mut self,
        fencing_token: u64,
        class: AwaitedExecutionFailureClass,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        if self.lease_expires_at.is_none_or(|expiry| now >= expiry) {
            return Err("awaited execution lease has expired".to_string());
        }
        let evidence = self
            .awaited_execution
            .as_mut()
            .ok_or_else(|| "awaited execution is not bound".to_string())?;
        if evidence.fencing_token != fencing_token
            || evidence.callback_action.is_none()
            || !matches!(
                evidence.phase,
                AwaitedExecutionPhase::ExecutionSucceeded | AwaitedExecutionPhase::ExecutionFailed
            )
        {
            return Err("stale or unresolved awaited callback owner".to_string());
        }
        evidence.callback_failure = Some(class);
        Ok(())
    }
}
