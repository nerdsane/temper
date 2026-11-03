use std::collections::BTreeMap;

use temper_runtime::persistence::PersistenceAppend;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

use super::{AtomicCompositeStream, DispatchError};

impl ServerState {
    /// One append per stream that has events. With `commits_rows`, each
    /// carries its record's query-plane row, which the batch commits with its
    /// events.
    pub(super) fn composite_appends(
        &self,
        tenant: &TenantId,
        commits_rows: bool,
        streams: &BTreeMap<String, AtomicCompositeStream>,
    ) -> Vec<PersistenceAppend> {
        streams
            .iter()
            .filter(|(_, stream)| !stream.events.is_empty())
            .map(|(persistence_id, stream)| PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: stream.expected_sequence,
                events: stream.events.clone(),
                state_timer: if commits_rows {
                    stream.state_timer.clone()
                } else {
                    None
                },
                projection: commits_rows
                    .then(|| self.append_projection(tenant, &stream.entity_type, &stream.state)),
            })
            .collect()
    }

    /// Index the streams a composite batch committed. Their query-plane rows
    /// committed with their events (`PersistenceAppend::projection`), so
    /// nothing is written here.
    pub(super) fn index_composite_streams(
        &self,
        tenant: &TenantId,
        streams: &BTreeMap<String, AtomicCompositeStream>,
    ) -> Result<(), DispatchError> {
        for stream in streams.values().filter(|stream| !stream.events.is_empty()) {
            self.cache_entity_status(tenant, &stream.state);
            self.index_composite_stream(tenant, stream)?;
            self.clear_commons_storage_projection_cache_for_entity(&stream.entity_type);
        }
        Ok(())
    }

    fn index_composite_stream(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> Result<(), DispatchError> {
        let index_key = format!("{tenant}:{}", stream.entity_type);
        let mut index = self.entity_index.write().map_err(|_| {
            DispatchError::Internal("entity index lock poisoned after composite batch".to_string())
        })?;
        index
            .entry(index_key)
            .or_default()
            .insert(stream.entity_id.clone());
        Ok(())
    }
}
