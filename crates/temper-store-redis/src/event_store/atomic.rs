//! Atomic Redis journal and snapshot scripts.

use super::*;

impl RedisEventStore {
    pub(super) fn encode_snapshot_source(
        source: &SnapshotSourceFence,
    ) -> Result<(u8, String), PersistenceError> {
        match source {
            SnapshotSourceFence::Unchecked => Ok((0, String::new())),
            SnapshotSourceFence::Absent => Ok((1, String::new())),
            SnapshotSourceFence::Exact { sequence_nr, state } => {
                let record = SnapshotRecord {
                    sequence_nr: *sequence_nr,
                    snapshot: state.clone(),
                };
                let encoded = serde_json::to_string(&record)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                Ok((2, encoded))
            }
        }
    }

    pub(super) async fn append_atomically(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        snapshot_source: &SnapshotSourceFence,
        batch_claim: Option<&PersistenceBatchIdempotency>,
    ) -> Result<(u64, bool), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        // A claimed batch must let the atomic Lua path inspect the durable
        // claim before optimistic concurrency. Exact retries necessarily carry
        // the pre-commit sequence and are stale after the first commit.
        if batch_claim.is_none() {
            let boundary = self.journal_boundary(persistence_id).await?;
            if boundary.latest_sequence != expected_sequence {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: expected_sequence,
                    actual: boundary.latest_sequence,
                });
            }
        }
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);
        let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
        let snapshot_key = Self::snapshot_key(tenant, entity_type, entity_id);
        let materialization_marker_key =
            Self::materialization_marker_key(tenant, entity_type, entity_id);
        let terminal_sequence_key = Self::terminal_sequence_key(tenant, entity_type, entity_id);
        let batch_idempotency_key = Self::batch_idempotency_key(batch_claim);

        let entity_ref = EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        let entity_ref_json = serde_json::to_string(&entity_ref)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let (snapshot_fence_mode, expected_snapshot) =
            Self::encode_snapshot_source(snapshot_source)?;
        let retire_snapshot = expected_sequence == 0
            && matches!(snapshot_source, SnapshotSourceFence::Exact { .. })
            && events.first().is_some_and(|event| {
                is_state_materialization_event_for(event, entity_type, entity_id)
            });

        let mut encoded_events = Vec::with_capacity(events.len());
        let mut sequence_nr = expected_sequence;
        let mut first_terminal_sequence = None;
        for event in events {
            sequence_nr = sequence_nr.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "Redis journal sequence overflow for {persistence_id}"
                ))
            })?;
            let mut envelope = event.clone();
            envelope.sequence_nr = sequence_nr;
            if first_terminal_sequence.is_none() && envelope.transitions_to_deleted() {
                first_terminal_sequence = Some(sequence_nr);
            }
            encoded_events.push(
                serde_json::to_string(&envelope)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
            );
        }

        let timestamp = sim_now().to_rfc3339();
        for _attempt in 0..APPEND_POINTER_RETRY_BUDGET {
            let current_segment_raw: Option<String> = self
                .client
                .get(&current_segment_key)
                .await
                .map_err(storage_error)?;
            let current_segment = match current_segment_raw {
                Some(raw) => raw.parse::<u64>().map_err(|error| {
                    PersistenceError::Storage(format!(
                        "invalid Redis segment pointer '{raw}' for {persistence_id}: {error}"
                    ))
                })?,
                None => 0,
            };
            let segment_key = Self::segment_key(tenant, entity_type, entity_id, current_segment);
            let canonical_segment_key = Self::segment_key(tenant, entity_type, entity_id, 0);
            let mut args = Vec::with_capacity(encoded_events.len() + 9);
            args.push(expected_sequence.to_string());
            args.push(entity_ref_json.clone());
            args.push(current_segment.to_string());
            args.push(timestamp.clone());
            args.push(snapshot_fence_mode.to_string());
            args.push(expected_snapshot.clone());
            args.push(u8::from(retire_snapshot).to_string());
            args.push(first_terminal_sequence.unwrap_or(0).to_string());
            args.push(
                batch_claim
                    .map(|claim| claim.intent_hash.clone())
                    .unwrap_or_default(),
            );
            args.extend(encoded_events.iter().cloned());

            let result: Vec<i64> = self
                .append_script
                .evalsha_with_reload(
                    &self.client,
                    vec![
                        seq_key.clone(),
                        events_key.clone(),
                        entities_key.clone(),
                        current_segment_key.clone(),
                        segment_key,
                        snapshot_key.clone(),
                        canonical_segment_key,
                        materialization_marker_key.clone(),
                        terminal_sequence_key.clone(),
                        batch_idempotency_key.clone(),
                    ],
                    args,
                )
                .await
                .map_err(storage_error)?;
            match result.as_slice() {
                [1, new_sequence] => return Ok((*new_sequence as u64, false)),
                [0, actual] => {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: *actual as u64,
                    });
                }
                [2, _current_segment] => continue,
                [3, _current_sequence] => {
                    return Err(PersistenceError::SnapshotGenerationChanged);
                }
                [4, current_sequence] => return Ok((*current_sequence as u64, true)),
                [5, _current_sequence] => {
                    return Err(PersistenceError::Storage(format!(
                        "atomic batch idempotency key '{}' was reused with a different intent",
                        batch_claim
                            .map(|claim| claim.idempotency_key.as_str())
                            .unwrap_or_default()
                    )));
                }
                other => {
                    return Err(PersistenceError::Storage(format!(
                        "unexpected Redis append script result: {other:?}"
                    )));
                }
            }
        }

        Err(PersistenceError::Storage(format!(
            "Redis append segment pointer for {persistence_id} did not stabilize after {APPEND_POINTER_RETRY_BUDGET} attempts"
        )))
    }

    pub(super) async fn save_snapshot_atomically(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let snapshot_key = Self::snapshot_key(tenant, entity_type, entity_id);
        let record = SnapshotRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;

        let history_key = Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
        let now = sim_now();
        let history = SnapshotHistoryRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
            created_at: now,
        };
        let encoded_history = serde_json::to_string(&history)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let (snapshot_fence_mode, expected_snapshot) = Self::encode_snapshot_source(source)?;

        let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
        let journal_sequence_key = Self::seq_key(tenant, entity_type, entity_id);
        let materialization_marker_key =
            Self::materialization_marker_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);
        let entity_ref = serde_json::to_string(&EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        })
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        for _attempt in 0..SNAPSHOT_POINTER_RETRY_BUDGET {
            let current_segment_raw: Option<String> = self
                .client
                .get(&current_segment_key)
                .await
                .map_err(storage_error)?;
            let current_segment = match current_segment_raw {
                Some(raw) => raw.parse::<u64>().map_err(|error| {
                    PersistenceError::Storage(format!(
                        "invalid Redis segment pointer '{raw}' for {persistence_id}: {error}"
                    ))
                })?,
                None => 0,
            };
            let segment_key = Self::segment_key(tenant, entity_type, entity_id, current_segment);
            let next_segment_key =
                Self::segment_key(tenant, entity_type, entity_id, current_segment + 1);
            let result: i64 = self
                .snapshot_script
                .evalsha_with_reload(
                    &self.client,
                    vec![
                        snapshot_key.clone(),
                        history_key.clone(),
                        current_segment_key.clone(),
                        segment_key,
                        next_segment_key,
                        journal_sequence_key.clone(),
                        materialization_marker_key.clone(),
                        entities_key.clone(),
                    ],
                    vec![
                        current_segment.to_string(),
                        sequence_nr.to_string(),
                        encoded.clone(),
                        encoded_history.clone(),
                        now.to_rfc3339(),
                        snapshot_fence_mode.to_string(),
                        expected_snapshot.clone(),
                        entity_ref.clone(),
                    ],
                )
                .await
                .map_err(storage_error)?;
            match result {
                0..=2 => return Ok(()),
                -1 => continue,
                -2 => return Err(PersistenceError::SnapshotGenerationChanged),
                other => {
                    return Err(PersistenceError::Storage(format!(
                        "Redis snapshot script returned unexpected result {other}"
                    )));
                }
            }
        }
        Err(PersistenceError::Storage(format!(
            "Redis snapshot segment pointer for {persistence_id} did not stabilize after {SNAPSHOT_POINTER_RETRY_BUDGET} attempts"
        )))
    }
}
