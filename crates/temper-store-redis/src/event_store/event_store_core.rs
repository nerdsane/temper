macro_rules! redis_event_store_core_methods {
    () => {
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
            let journals_key = Self::tenant_journals_key(tenant);

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
            args.push(Self::journal_member(entity_type, entity_id));
            let unscoped = split_scoped_journal_entity_id(entity_id).is_none();
            args.push(
                unscoped
                    .then(|| encode_lex_component(entity_id))
                    .unwrap_or_default(),
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
                journals_key,
                Self::unscoped_journals_key(tenant, entity_type),
                Self::unscoped_generation_key(tenant, entity_type),
                Self::unscoped_fence_key(tenant, entity_type),
            ];
            let result: Vec<i64> = self
                .append_script
                .evalsha_with_reload(&self.client, keys, args)
                .await
                .map_err(storage_error)?;

            match result.as_slice() {
                [1, new_seq] => {
                    let new_seq = *new_seq as u64;
                    self.update_segment_after_append(
                        tenant,
                        entity_type,
                        entity_id,
                        expected_sequence,
                        new_seq,
                    )
                    .await?;
                    Ok(new_seq)
                }
                [0, actual] => Err(PersistenceError::ConcurrencyViolation {
                    expected: expected_sequence,
                    actual: *actual as u64,
                }),
                [-1, _] => Err(PersistenceError::Storage(
                    "stream descriptor publication fence".into(),
                )),
                other => Err(PersistenceError::Storage(format!(
                    "unexpected Lua script result: {other:?}"
                ))),
            }
        }

        async fn append_batch(
            &self,
            appends: &[PersistenceAppend],
        ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
            if appends.is_empty() {
                return Ok(Vec::new());
            }
            // Redis has no query-plane projections; match `append_with_index_rows`
            // by accepting and intentionally ignoring derived projection rows.
            if let [append] = appends {
                let sequence_nr = self
                    .append(
                        &append.persistence_id,
                        append.expected_sequence,
                        &append.events,
                    )
                    .await?;
                return Ok(vec![PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr,
                }]);
            }

            let mut seen = std::collections::BTreeSet::new();
            let mut parsed = Vec::with_capacity(appends.len());
            let mut tenant_name: Option<&str> = None;
            for append in appends {
                if !seen.insert(append.persistence_id.as_str()) {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate persistence_id '{}' in append_batch",
                        append.persistence_id
                    )));
                }
                let (tenant, entity_type, entity_id) =
                    parse_persistence_id_parts(&append.persistence_id)
                        .map_err(PersistenceError::Storage)?;
                if tenant_name.is_some_and(|expected| expected != tenant) {
                    return Err(PersistenceError::Storage(
                        "append_batch cannot span Redis tenant indexes".to_string(),
                    ));
                }
                tenant_name = Some(tenant);
                parsed.push((tenant, entity_type, entity_id));
            }
            let tenant = tenant_name.expect("non-empty appends set tenant");
            let mut keys = Vec::with_capacity(appends.len() * 5 + 2);
            let mut args = Vec::new();
            args.push(appends.len().to_string());
            for (append, (_, entity_type, entity_id)) in appends.iter().zip(parsed.iter()) {
                keys.push(Self::seq_key(tenant, entity_type, entity_id));
                keys.push(Self::events_key(tenant, entity_type, entity_id));
                keys.push(Self::unscoped_journals_key(tenant, entity_type));
                keys.push(Self::unscoped_generation_key(tenant, entity_type));
                keys.push(Self::unscoped_fence_key(tenant, entity_type));
                args.push(append.expected_sequence.to_string());
                let entity_ref = EntityRef {
                    entity_type: (*entity_type).to_string(),
                    entity_id: (*entity_id).to_string(),
                };
                args.push(
                    serde_json::to_string(&entity_ref)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                );
                args.push(Self::journal_member(entity_type, entity_id));
                args.push(
                    split_scoped_journal_entity_id(entity_id)
                        .is_none()
                        .then(|| encode_lex_component(entity_id))
                        .unwrap_or_default(),
                );
                args.push(append.events.len().to_string());
                let mut sequence = append.expected_sequence;
                for event in &append.events {
                    sequence += 1;
                    let mut stored = event.clone();
                    stored.sequence_nr = sequence;
                    args.push(
                        serde_json::to_string(&stored)
                            .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                    );
                }
            }
            keys.push(Self::tenant_entities_key(tenant));
            keys.push(Self::tenant_journals_key(tenant));
            let result: Vec<i64> = self
                .append_batch_script
                .evalsha_with_reload(&self.client, keys, args)
                .await
                .map_err(storage_error)?;
            if result.first() == Some(&-1) {
                return Err(PersistenceError::Storage(
                    "stream descriptor publication fence".into(),
                ));
            }
            if result.first() == Some(&0) {
                let [_, append_index, actual] = result.as_slice() else {
                    return Err(PersistenceError::Storage(format!(
                        "unexpected Redis append_batch conflict result: {result:?}"
                    )));
                };
                let index = usize::try_from(*append_index)
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .filter(|index| *index < appends.len())
                    .ok_or_else(|| {
                        PersistenceError::Storage(
                            "Redis append_batch returned an invalid conflict index".to_string(),
                        )
                    })?;
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: appends[index].expected_sequence,
                    actual: *actual as u64,
                });
            }
            if result.len() != appends.len() + 1 || result.first() != Some(&1) {
                return Err(PersistenceError::Storage(format!(
                    "unexpected Redis append_batch result: {result:?}"
                )));
            }

            let mut results = Vec::with_capacity(appends.len());
            for (((append, (tenant, entity_type, entity_id)), new_sequence), result_index) in
                appends
                    .iter()
                    .zip(parsed.iter())
                    .zip(result.iter().skip(1))
                    .zip(0..appends.len())
            {
                let new_sequence = u64::try_from(*new_sequence).map_err(|_| {
                    PersistenceError::Storage(format!(
                        "Redis append_batch returned a negative sequence at index {result_index}"
                    ))
                })?;
                self.update_segment_after_append(
                    tenant,
                    entity_type,
                    entity_id,
                    append.expected_sequence,
                    new_sequence,
                )
                .await?;
                results.push(PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr: new_sequence,
                });
            }
            Ok(results)
        }

        async fn read_events(
            &self,
            persistence_id: &str,
            from_sequence: u64,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            let events_key = Self::events_key(tenant, entity_type, entity_id);

            // Events are stored via RPUSH with sequential indices starting at 0.
            // Event at index i has sequence_nr = i + 1.
            // To read events with sequence_nr > from_sequence, start at index from_sequence.
            let start_index = from_sequence as i64;
            let encoded_events: Vec<String> = self
                .client
                .lrange(&events_key, start_index, -1)
                .await
                .map_err(storage_error)?;

            let mut out = Vec::with_capacity(encoded_events.len());
            for encoded in encoded_events {
                let env: PersistenceEnvelope = serde_json::from_str(&encoded)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                out.push(env);
            }
            out.sort_by_key(|e| e.sequence_nr);
            Ok(out)
        }

        async fn read_events_limited(
            &self,
            persistence_id: &str,
            from_sequence: u64,
            limit: usize,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            let events_key = Self::events_key(tenant, entity_type, entity_id);
            let end = from_sequence
                .saturating_add(limit as u64)
                .saturating_sub(1)
                .min(i64::MAX as u64) as i64;
            let encoded_events: Vec<String> = self
                .client
                .lrange(&events_key, from_sequence.min(i64::MAX as u64) as i64, end)
                .await
                .map_err(storage_error)?;
            encoded_events
                .into_iter()
                .map(|encoded| {
                    serde_json::from_str(&encoded)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))
                })
                .collect()
        }

        async fn read_latest_events(
            &self,
            persistence_id: &str,
            limit: usize,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            let events_key = Self::events_key(tenant, entity_type, entity_id);
            let start = -(limit.min(i64::MAX as usize) as i64);
            let encoded_events: Vec<String> = self
                .client
                .lrange(&events_key, start, -1)
                .await
                .map_err(storage_error)?;
            encoded_events
                .into_iter()
                .map(|encoded| {
                    serde_json::from_str(&encoded)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))
                })
                .collect()
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
            let _: () = self
                .client
                .set(&key, encoded, None, None, false)
                .await
                .map_err(storage_error)?;

            let history_key =
                Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
            let history = SnapshotHistoryRecord {
                sequence_nr,
                snapshot: snapshot.to_vec(),
                created_at: chrono::Utc::now(),
            };
            let encoded_history = serde_json::to_string(&history)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let _: () = self
                .client
                .set(&history_key, encoded_history, None, None, false)
                .await
                .map_err(storage_error)?;

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
            let key = Self::tenant_entities_key(tenant);
            let members: Vec<String> = self.client.smembers(&key).await.map_err(storage_error)?;

            let mut out = Vec::with_capacity(members.len());
            for encoded in members {
                let entity_ref: EntityRef = serde_json::from_str(&encoded)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                out.push((entity_ref.entity_type, entity_ref.entity_id));
            }

            out.sort();
            out.dedup();
            Ok(out)
        }

        async fn list_entity_ids_by_type(
            &self,
            tenant: &str,
            entity_type: &str,
        ) -> Result<Vec<String>, PersistenceError> {
            let key = Self::tenant_entities_key(tenant);
            let members: Vec<String> = self.client.smembers(&key).await.map_err(storage_error)?;

            let mut out = Vec::new();
            for encoded in members {
                let entity_ref: EntityRef = serde_json::from_str(&encoded)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                if entity_ref.entity_type == entity_type {
                    out.push(entity_ref.entity_id);
                }
            }

            out.sort();
            out.dedup();
            Ok(out)
        }
    };
}
