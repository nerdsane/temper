//! Terminal join fencing and retry mutations.

use super::super::model::*;

impl CollectionWorkflowRecordV1 {
    /// Fence the sole terminal join delivery after exact classification.
    pub(crate) fn begin_join(
        &mut self,
        delivery_id: String,
    ) -> Result<CollectionMutationOutcome, String> {
        if !self.status.is_terminal() || self.terminal_classification != Some(self.status) {
            return Err("join requires a terminal classified workflow".to_string());
        }
        if let Some(existing) = &self.join_delivery_id {
            return if existing == &delivery_id {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("workflow already has a different join delivery".to_string())
            };
        }
        if delivery_id.is_empty() {
            return Err("join delivery identity must be non-empty".to_string());
        }
        self.join_delivery_id = Some(delivery_id);
        self.join_status = CollectionJoinStatus::InFlight;
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Record the sole join target receipt or its bounded terminal failure.
    pub(crate) fn record_join_terminal(
        &mut self,
        delivery_id: &str,
        delivered: bool,
    ) -> Result<CollectionMutationOutcome, String> {
        if self.join_delivery_id.as_deref() != Some(delivery_id) {
            return Err("join result names a different delivery".to_string());
        }
        let target = if delivered {
            CollectionJoinStatus::Delivered
        } else {
            CollectionJoinStatus::DeliveryFailed
        };
        if self.join_status == target {
            return Ok(CollectionMutationOutcome::Replayed);
        }
        if self.join_status != CollectionJoinStatus::InFlight {
            return Err("join delivery is not in flight".to_string());
        }
        self.join_status = target;
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Reopen only a failed join for one governed manual delivery retry.
    pub(crate) fn record_join_retry(
        &mut self,
        delivery_id: &str,
    ) -> Result<CollectionMutationOutcome, String> {
        if self.join_delivery_id.as_deref() != Some(delivery_id) {
            return Err("join retry names a different delivery".to_string());
        }
        if self.join_status == CollectionJoinStatus::InFlight {
            return Ok(CollectionMutationOutcome::Replayed);
        }
        if self.join_status != CollectionJoinStatus::DeliveryFailed {
            return Err("only a failed collection join can be retried".to_string());
        }
        self.join_status = CollectionJoinStatus::InFlight;
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Fence an unresolved join when the source commits a newer workflow.
    pub(crate) fn supersede_join(&mut self) -> Result<CollectionMutationOutcome, String> {
        if self.join_status == CollectionJoinStatus::SupersededByNewWorkflow {
            return Ok(CollectionMutationOutcome::Replayed);
        }
        if !matches!(
            self.join_status,
            CollectionJoinStatus::InFlight | CollectionJoinStatus::DeliveryFailed
        ) {
            return Err("only an unresolved collection join can be superseded".to_string());
        }
        self.join_status = CollectionJoinStatus::SupersededByNewWorkflow;
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }
}
