//! Redis implementation of the runtime event-store contract.

use super::*;

impl EventStore for RedisEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);
        let live_entities_key = Self::tenant_live_entities_key(tenant);
        let typed_live_entities_key = Self::typed_live_entities_key(tenant, entity_type);
        let tombstones_key = Self::tenant_tombstones_key(tenant);
        let index_complete_key = Self::entity_index_complete_key(tenant);

        // Pre-serialize events with provisional sequence numbers.
        let mut args: Vec<String> = Vec::with_capacity(events.len() + 4);
        args.push(expected_sequence.to_string());

        let entity_ref = EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        let entity_ref_json = serde_json::to_string(&entity_ref)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        args.push(entity_ref_json);
        args.push(entity_id.to_string());
        args.push(
            if events.iter().any(is_entity_tombstone) {
                "1"
            } else {
                "0"
            }
            .to_string(),
        );

        let mut seq = expected_sequence;
        for event in events {
            seq += 1;
            let mut env = event.clone();
            env.sequence_nr = seq;
            let encoded = serde_json::to_string(&env)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            args.push(encoded);
        }

        let keys = vec![
            seq_key,
            events_key,
            entities_key,
            live_entities_key,
            typed_live_entities_key,
            tombstones_key,
            index_complete_key,
            Self::entity_index_event_cursor_key(tenant, &args[1]),
            Self::entity_index_discovered_key(tenant),
        ];
        let result: Vec<i64> = self
            .append_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;

        match result.as_slice() {
            [1, new_seq] => {
                let new_seq = *new_seq as u64;
                if let Err(error) = self
                    .write_append_segment_metadata(
                        tenant,
                        entity_type,
                        entity_id,
                        expected_sequence,
                        new_seq,
                    )
                    .await
                {
                    tracing::error!(
                        tenant,
                        entity_type,
                        entity_id,
                        new_seq,
                        error = %error,
                        "journal append committed but advisory segment metadata update failed"
                    );
                }
                Ok(new_seq)
            }
            [0, actual] => Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: *actual as u64,
            }),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Lua script result: {other:?}"
            ))),
        }
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        match appends {
            [] => Ok(Vec::new()),
            [append] => {
                let sequence_nr = self
                    .append(
                        &append.persistence_id,
                        append.expected_sequence,
                        &append.events,
                    )
                    .await?;
                Ok(vec![PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr,
                }])
            }
            _ => Err(PersistenceError::Storage(
                "RedisEventStore does not support atomic multi-journal append_batch".to_string(),
            )),
        }
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_head(persistence_id, from_sequence)
            .await
            .map(|read| read.events)
    }

    async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let events_key = Self::events_key(tenant, entity_type, entity_id);

        // Events are stored via RPUSH with sequential indices starting at 0.
        // Event at index i has sequence_nr = i + 1.
        // To read events with sequence_nr > from_sequence, start at index from_sequence.
        let encoded_read: Vec<String> = self
            .read_events_with_head_script
            .evalsha_with_reload(
                &self.client,
                vec![seq_key, events_key],
                vec![from_sequence.to_string()],
            )
            .await
            .map_err(storage_error)?;
        let (head, encoded_events) = encoded_read.split_first().ok_or_else(|| {
            PersistenceError::Storage(format!(
                "journal read returned no head for {persistence_id}"
            ))
        })?;
        let journal_head_sequence_nr = head.parse::<u64>().map_err(|error| {
            PersistenceError::Serialization(format!(
                "invalid journal head for {persistence_id}: {error}"
            ))
        })?;

        let mut out = Vec::with_capacity(encoded_events.len());
        for encoded in encoded_events {
            let env: PersistenceEnvelope = serde_json::from_str(encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            out.push(env);
        }
        out.sort_by_key(|e| e.sequence_nr);
        Ok(JournalRead {
            events: out,
            journal_head_sequence_nr,
        })
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let record = SnapshotRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let history_key = Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
        let history = SnapshotHistoryRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
            created_at: chrono::Utc::now(),
        };
        let encoded_history = serde_json::to_string(&history)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let saved: i64 = self
            .save_snapshot_script
            .evalsha_with_reload(
                &self.client,
                vec![key, history_key],
                vec![encoded, encoded_history],
            )
            .await
            .map_err(storage_error)?;
        debug_assert_eq!(saved, 1, "snapshot save script must report one write");

        let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
        let current_segment_raw: Option<String> = self
            .client
            .get(&current_segment_key)
            .await
            .map_err(storage_error)?;
        let current_segment = current_segment_raw
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        let segment_key = Self::segment_key(tenant, entity_type, entity_id, current_segment);
        let existing: Option<String> =
            self.client.get(&segment_key).await.map_err(storage_error)?;
        let mut segment = existing
            .as_deref()
            .map(serde_json::from_str::<SegmentRecord>)
            .transpose()
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?
            .unwrap_or_else(|| SegmentRecord {
                segment_index: current_segment,
                start_sequence_nr: 1,
                end_sequence_nr: Some(sequence_nr),
                snapshot_sequence: Some(sequence_nr),
                event_count: sequence_nr,
                sealed_at: None,
                created_at: chrono::Utc::now(),
            });
        segment.end_sequence_nr = Some(sequence_nr);
        segment.snapshot_sequence = Some(sequence_nr);
        segment.event_count = sequence_nr.saturating_sub(segment.start_sequence_nr) + 1;
        segment.sealed_at = Some(chrono::Utc::now());
        let encoded_segment = serde_json::to_string(&segment)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&segment_key, encoded_segment, None, None, false)
            .await
            .map_err(storage_error)?;

        let next_segment = current_segment + 1;
        let next_segment_key = Self::segment_key(tenant, entity_type, entity_id, next_segment);
        let next = SegmentRecord {
            segment_index: next_segment,
            start_sequence_nr: sequence_nr + 1,
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed_at: None,
            created_at: chrono::Utc::now(),
        };
        let encoded_next = serde_json::to_string(&next)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&next_segment_key, encoded_next, None, None, false)
            .await
            .map_err(storage_error)?;
        let _: () = self
            .client
            .set(
                &current_segment_key,
                next_segment.to_string(),
                None,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let current_encoded: Option<String> = self.client.get(&key).await.map_err(storage_error)?;
        let current_encoded = current_encoded.ok_or_else(|| {
            PersistenceError::Storage(format!(
                "cannot replace missing snapshot at sequence {sequence_nr} for {persistence_id}"
            ))
        })?;
        let current: SnapshotRecord = serde_json::from_str(&current_encoded)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        if current.sequence_nr != sequence_nr || current.snapshot != expected_snapshot {
            return Err(PersistenceError::Storage(format!(
                "snapshot changed while replacing sequence {sequence_nr} for {persistence_id}"
            )));
        }

        let replacement = SnapshotRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
        };
        let encoded_replacement = serde_json::to_string(&replacement)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let history_key = Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
        let history = SnapshotHistoryRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
            created_at: chrono::Utc::now(),
        };
        let encoded_history = serde_json::to_string(&history)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let replaced: i64 = self
            .replace_snapshot_script
            .evalsha_with_reload(
                &self.client,
                vec![key, history_key],
                vec![current_encoded, encoded_history, encoded_replacement],
            )
            .await
            .map_err(storage_error)?;
        if replaced != 1 {
            return Err(PersistenceError::Storage(format!(
                "snapshot changed while replacing sequence {sequence_nr} for {persistence_id}"
            )));
        }
        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let encoded: Option<String> = self.client.get(&key).await.map_err(storage_error)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let record: SnapshotRecord = serde_json::from_str(&encoded)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        Ok(Some((record.sequence_nr, record.snapshot)))
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.ensure_entity_index_complete(tenant).await?;
        let members: Vec<String> = self
            .client
            .zrange(
                Self::tenant_live_entities_key(tenant),
                0,
                -1,
                None,
                false,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;

        let entity_refs = Self::decode_entity_refs(members)?;
        let mut entity_refs = self
            .revalidate_live_entities(tenant, entity_refs, true)
            .await?;
        entity_refs.sort();
        entity_refs.dedup();
        Ok(entity_refs
            .into_iter()
            .map(|entity_ref| (entity_ref.entity_type, entity_ref.entity_id))
            .collect())
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.ensure_entity_index_complete(tenant).await?;
        let entity_ids: Vec<String> = self
            .client
            .zrange(
                Self::typed_live_entities_key(tenant, entity_type),
                0,
                -1,
                None,
                false,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;
        let entity_refs = entity_ids
            .into_iter()
            .map(|entity_id| EntityRef {
                entity_type: entity_type.to_string(),
                entity_id,
            })
            .collect();
        Ok(self
            .revalidate_live_entities(tenant, entity_refs, true)
            .await?
            .into_iter()
            .map(|entity_ref| entity_ref.entity_id)
            .collect())
    }

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        // Legacy tenants are migrated incrementally so the bounded API never
        // performs SMEMBERS or transfers every historical journal. New tenants
        // have a complete index from their first append.
        let complete = self
            .migrate_entity_index_page(tenant, limit.clamp(64, 1024))
            .await?;
        if !complete {
            return Err(PersistenceError::Storage(
                "legacy Redis entity index migration incomplete; retry".to_string(),
            ));
        }
        let end = limit.saturating_sub(1).min(i64::MAX as usize) as i64;
        if let Some(entity_type) = entity_type {
            let ids: Vec<String> = self
                .client
                .zrange(
                    Self::typed_live_entities_key(tenant, entity_type),
                    0,
                    end,
                    None,
                    false,
                    None,
                    false,
                )
                .await
                .map_err(storage_error)?;
            let entity_refs = ids
                .into_iter()
                .map(|entity_id| EntityRef {
                    entity_type: entity_type.to_string(),
                    entity_id,
                })
                .collect();
            return Ok(self
                .revalidate_live_entities(tenant, entity_refs, false)
                .await?
                .into_iter()
                .map(|entity_ref| (entity_ref.entity_type, entity_ref.entity_id))
                .collect());
        }

        let members: Vec<String> = self
            .client
            .zrange(
                Self::tenant_live_entities_key(tenant),
                0,
                end,
                None,
                false,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;
        let entity_refs = Self::decode_entity_refs(members)?;
        Ok(self
            .revalidate_live_entities(tenant, entity_refs, false)
            .await?
            .into_iter()
            .map(|entity_ref| (entity_ref.entity_type, entity_ref.entity_id))
            .collect())
    }
}
