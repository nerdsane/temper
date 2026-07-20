//! Best-effort derived segment metadata for committed Redis appends.

use fred::prelude::*;
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;

use super::{RedisEventStore, SegmentRecord};

impl RedisEventStore {
    pub(super) async fn write_append_segment_metadata(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        new_sequence: u64,
    ) -> Result<(), PersistenceError> {
        let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
        let current_segment_raw: Option<String> = self
            .client
            .get(&current_segment_key)
            .await
            .map_err(storage_error)?;
        let segment_index = current_segment_raw
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        let segment_key = Self::segment_key(tenant, entity_type, entity_id, segment_index);
        let existing: Option<String> =
            self.client.get(&segment_key).await.map_err(storage_error)?;
        let mut record = existing
            .as_deref()
            .map(serde_json::from_str::<SegmentRecord>)
            .transpose()
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?
            .unwrap_or_else(|| SegmentRecord {
                segment_index,
                start_sequence_nr: (expected_sequence + 1).max(1),
                end_sequence_nr: None,
                snapshot_sequence: None,
                event_count: 0,
                sealed_at: None,
                created_at: sim_now(),
            });
        record.end_sequence_nr = Some(new_sequence);
        record.event_count = new_sequence.saturating_sub(record.start_sequence_nr) + 1;
        let encoded = serde_json::to_string(&record)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let _: () = self
            .client
            .set(&segment_key, encoded, None, None, false)
            .await
            .map_err(storage_error)?;
        let _: () = self
            .client
            .set(
                &current_segment_key,
                segment_index.to_string(),
                None,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;
        Ok(())
    }
}
