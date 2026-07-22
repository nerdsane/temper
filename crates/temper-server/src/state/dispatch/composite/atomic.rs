use super::*;

impl crate::state::ServerState {
    pub(super) async fn apply_composite_sub_writes_atomic(
        &self,
        parent: AtomicCompositeParent<'_>,
        prepared_sub_writes: &[PreparedCompositeSubWrite],
        batch_claim: PersistenceBatchIdempotency,
        batch_already_committed: bool,
    ) -> Result<bool, DispatchError> {
        let tenant = parent.tenant;
        let parent_entity_type = parent.entity_type;
        let parent_entity_id = parent.entity_id;
        let parent_action = parent.action;
        let parent_idempotency = parent.idempotency;

        let Some((store, backend)) = self.event_journal() else {
            return Ok(false);
        };
        if prepared_sub_writes.is_empty() {
            return Ok(true);
        }

        // Capture one coherent table per participating entity type before any
        // recovery or staging await point. Every event, key row, and activation
        // token in this atomic attempt is derived from that same table.
        let mut captured_tables = BTreeMap::new();
        if parent.record_event {
            captured_tables.insert(
                parent_entity_type.to_string(),
                self.transition_table_for_dispatch(tenant, parent_entity_type)?,
            );
        }
        for write in prepared_sub_writes {
            if !captured_tables.contains_key(&write.entity_type) {
                captured_tables.insert(
                    write.entity_type.clone(),
                    self.transition_table_for_dispatch(tenant, &write.entity_type)?,
                );
            }
        }

        let field_sync_mode = self.composite_batch_field_sync_mode(tenant, backend);
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let mut streams: BTreeMap<String, AtomicCompositeStream> = BTreeMap::new();
        let parent_persistence_id = format!("{tenant}:{parent_entity_type}:{parent_entity_id}");
        let mut composite_event = build_composite_event(
            tenant,
            parent_entity_type,
            parent_entity_id,
            parent_action,
            parent_idempotency,
            prepared_sub_writes,
        );
        composite_event
            .intent_hash
            .clone_from(&batch_claim.intent_hash);
        composite_event
            .composite_idempotency_key
            .clone_from(&batch_claim.idempotency_key);
        let timing_enabled = prepared_sub_writes.len() >= 10;
        let total_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        let parent_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only

        if parent.record_event {
            self.ensure_atomic_composite_stream(
                &mut streams,
                &store,
                backend,
                tenant,
                parent_entity_type,
                parent_entity_id,
                captured_tables
                    .get(parent_entity_type)
                    .expect("parent table captured before stream recovery"),
                None,
                false,
            )
            .await?;
            let stream = streams
                .get_mut(&parent_persistence_id)
                .expect("parent stream inserted before composite event append");
            if !batch_already_committed {
                let parent_audit_already_persisted = stream
                    .state
                    .has_processed_idempotency_key(parent_idempotency);
                if !parent_audit_already_persisted && !stream.state.can_accept_event() {
                    return Err(DispatchError::Internal(format!(
                        "composite {parent_entity_type}.{parent_action} parent audit would exceed the event budget for {parent_entity_type}:{parent_entity_id}"
                    )));
                }
                if !parent_audit_already_persisted {
                    stream.events.push(composite_event_envelope(
                        &parent_persistence_id,
                        &composite_event,
                    )?);
                    stream.state.sequence_nr = stream.state.sequence_nr.saturating_add(1);
                    stream.state.record_internal_envelope();
                    stream.state.record_durable_idempotency_key(
                        parent_idempotency,
                        stream.state.sequence_nr,
                    );
                }
            }
        }
        let parent_ms = parent_started_at.map(|started| started.elapsed().as_millis() as u64);

        let stage_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        let mut post_commit = Vec::with_capacity(prepared_sub_writes.len());
        for write in prepared_sub_writes {
            let persistence_id = format!("{tenant}:{}:{}", write.entity_type, write.entity_id);
            self.ensure_atomic_composite_stream(
                &mut streams,
                &store,
                backend,
                tenant,
                &write.entity_type,
                &write.entity_id,
                captured_tables
                    .get(&write.entity_type)
                    .expect("sub-write table captured before stream recovery"),
                write.preflight_target.as_ref(),
                write.uses_parent_gate && write.action == "Create",
            )
            .await?;

            if batch_already_committed {
                continue;
            }

            let table = streams
                .get(&persistence_id)
                .expect("stream inserted before table lookup")
                .table
                .clone();
            let cross_entity_booleans =
                if table_has_cross_entity_guards_for_action(&table, &write.action) {
                    self.resolve_cross_entity_guards(
                        tenant,
                        &write.entity_type,
                        &write.entity_id,
                        &write.action,
                    )
                    .await
                } else {
                    BTreeMap::new()
                };
            let stream = streams
                .get_mut(&persistence_id)
                .expect("stream inserted before processing sub-write");

            let incomplete_pack_object_repair =
                is_incomplete_existing_pack_object_create(write, stream);

            if should_skip_existing_pack_object_create(write, stream) {
                continue;
            }

            if !incomplete_pack_object_repair
                && stream
                    .state
                    .has_processed_idempotency_key(&write.idempotency_key)
            {
                continue;
            }

            validate_composite_ref_compare_and_set(
                parent_entity_type,
                parent_action,
                write,
                stream,
            )?;

            let result = process_action_with_xref_and_field_mode(
                &mut stream.state,
                &table,
                &write.action,
                &write.params,
                &cross_entity_booleans,
                field_sync_mode,
            );
            if !result.success {
                return Err(DispatchError::Internal(result.error.unwrap_or_else(|| {
                    format!(
                        "composite {parent_entity_type}.{parent_action} sub-write {} failed during atomic staging",
                        write.idx
                    )
                })));
            }
            if !result.custom_effects.is_empty()
                || !result.scheduled_actions.is_empty()
                || !result.spawn_requests.is_empty()
            {
                // These obligations are owned by actor idempotency and are not
                // represented in the atomic batch claim. Decline before the
                // append so an ambiguous successful commit cannot make their
                // first execution indistinguishable from a completed replay.
                return Ok(false);
            }
            if !result.overflow_blobs.is_empty() {
                let blob_store = blob_store.as_ref().ok_or_else(|| {
                    DispatchError::Internal(
                        "field-overflow blobs require a configured object blob store".to_string(),
                    )
                })?;
                crate::blobs::put_overflow_blobs(blob_store, &result.overflow_blobs)
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "field-overflow blob persistence failed during composite batch: {e}"
                        ))
                    })?;
            }

            let mut event = result
                .event
                .expect("successful process_action returns an event");
            event.idempotency_key = Some(write.idempotency_key.clone());
            stream
                .events
                .push(composite_envelope(&persistence_id, &event)?);
            stream.state.sequence_nr = stream.state.sequence_nr.saturating_add(1);
            stream.state.push_event_bounded(event);
            post_commit.push(AtomicCompositePostCommit {
                entity_type: write.entity_type.clone(),
                entity_id: write.entity_id.clone(),
                action: write.action.clone(),
                params: write.params.clone(),
                idempotency_key: write.idempotency_key.clone(),
                response: EntityResponse {
                    success: true,
                    state: stream.state.clone(),
                    error: None,
                    custom_effects: result.custom_effects,
                    scheduled_actions: result.scheduled_actions,
                    spawn_requests: result.spawn_requests,
                    spec_governed: true,
                },
            });
        }
        let stage_ms = stage_started_at.map(|started| started.elapsed().as_millis() as u64);

        let mut appends = Vec::with_capacity(streams.len());
        for (persistence_id, stream) in &mut streams {
            let mut events = Vec::with_capacity(
                stream.events.len() + usize::from(stream.materialization_baseline.is_some()),
            );
            if let Some(baseline) = stream.materialization_baseline.take() {
                events.push(
                    state_materialization_envelope(persistence_id, &baseline, sim_now())
                        .map_err(composite_batch_persistence_error)?,
                );
                stream.state.sequence_nr = stream.state.sequence_nr.saturating_add(1);
            }
            events.extend(stream.events.iter().cloned());
            // Empty is an exact declared-key set and must delete ownership
            // inherited from an older, keyed contract.
            let reconcile_keys = true;
            let key_set_signature = crate::key_index::declared_key_write_contract(&stream.table);
            let key_rows = crate::key_index::derive_entity_key_rows(
                &stream.table.keys,
                &stream.state.fields,
                stream.state.status != "Deleted",
            );
            appends.push(PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: stream.expected_sequence,
                events,
                key_rows,
                reconcile_keys,
                key_set_signature: Some(key_set_signature),
                snapshot_source: stream.snapshot_source.clone(),
                batch_idempotency: None,
            });
        }
        debug_assert!(
            !appends.is_empty(),
            "a non-empty composite must capture at least one target stream"
        );
        appends[0].batch_idempotency = Some(batch_claim);

        let append_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        let append_results = store
            .append_batch(&appends)
            .await
            .map_err(composite_batch_persistence_error)?;
        let append_ms = append_started_at.map(|started| started.elapsed().as_millis() as u64);
        let batch_replayed = append_results
            .iter()
            .any(|result| result.batch_already_applied);
        if batch_already_committed && !batch_replayed {
            return Err(DispatchError::Internal(
                "durable composite claim disappeared between retry probe and atomic replay"
                    .to_string(),
            ));
        }

        if !batch_replayed {
            for effect in post_commit {
                let mut sub_agent_ctx = parent.agent_ctx.clone();
                sub_agent_ctx.idempotency_key = Some(effect.idempotency_key.clone());
                let context = PostDispatchContext {
                    tenant,
                    entity_type: &effect.entity_type,
                    entity_id: &effect.entity_id,
                    action: &effect.action,
                    agent_ctx: &sub_agent_ctx,
                    dispatch_idempotency_key: Some(&effect.idempotency_key),
                    action_params: &effect.params,
                    await_integration: false,
                };
                self.run_post_dispatch_effects(&context, effect.response)
                    .await;
            }
        }

        let projection_collect_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        self.update_composite_query_projections(tenant, &streams)
            .await?;
        let projection_collect_ms =
            projection_collect_started_at.map(|started| started.elapsed().as_millis() as u64);

        let projection_write_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        let projection_write_ms =
            projection_write_started_at.map(|started| started.elapsed().as_millis() as u64);

        let reload_started_at = timing_enabled.then(std::time::Instant::now); // determinism-ok: observability only
        for stream in streams.values() {
            if !stream.target_existed {
                continue;
            }
            self.stop_and_remove_entity(tenant, &stream.entity_type, &stream.entity_id);
            if stream.state.status == "Deleted" {
                continue;
            }
            if !self
                .ensure_entity_loaded(tenant, &stream.entity_type, &stream.entity_id)
                .await
            {
                return Err(DispatchError::Internal(format!(
                    "composite batch committed {}:{} but failed to reload it",
                    stream.entity_type, stream.entity_id
                )));
            }
        }
        let reload_ms = reload_started_at.map(|started| started.elapsed().as_millis() as u64);
        if let Some(started) = total_started_at {
            tracing::info!(
                tenant = %tenant,
                parent_entity_type,
                parent_entity_id,
                parent_action,
                sub_writes = prepared_sub_writes.len(),
                streams = streams.len(),
                parent_ms = parent_ms.unwrap_or_default(),
                stage_ms = stage_ms.unwrap_or_default(),
                append_ms = append_ms.unwrap_or_default(),
                projection_collect_ms = projection_collect_ms.unwrap_or_default(),
                projection_write_ms = projection_write_ms.unwrap_or_default(),
                reload_ms = reload_ms.unwrap_or_default(),
                batch_replayed,
                total_ms = started.elapsed().as_millis() as u64,
                "composite atomic batch timing"
            );
        }

        Ok(true)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "composite stream capture is an explicit multi-entity transaction boundary"
    )]
    async fn ensure_atomic_composite_stream(
        &self,
        streams: &mut BTreeMap<String, AtomicCompositeStream>,
        store: &crate::storage::BoxedEventStore,
        backend: BackendLabel,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        table: &TransitionTable,
        preflight_target: Option<&PreflightCompositeTarget>,
        suppress_bootstrap_event: bool,
    ) -> Result<(), DispatchError> {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        if streams.contains_key(&persistence_id) {
            return Ok(());
        }

        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let initial_fields = serde_json::json!({});
        let source = recover_entity_state_from_stable_sources(EntityRecoveryContext {
            tenant: tenant.as_str(),
            entity_type,
            entity_id,
            table,
            store,
            backend,
            initial_fields: &initial_fields,
            blob_store: blob_store.as_ref(),
        })
        .await
        .map_err(|error| {
            DispatchError::Internal(format!(
                "failed to capture durable composite source for {entity_type}:{entity_id}: {error}"
            ))
        })?;
        let expected_sequence = source.journal_sequence;
        let has_snapshot = source.snapshot.is_some();
        let snapshot_source = match source.snapshot {
            Some(snapshot) => SnapshotSourceFence::Exact {
                sequence_nr: snapshot.sequence_nr,
                state: snapshot.state,
            },
            None => SnapshotSourceFence::Absent,
        };
        let (target_exists, mut state) = if let Some(state) = source.state {
            (true, state)
        } else {
            (
                false,
                synthetic_initial_state(entity_type, entity_id, table),
            )
        };
        // Composite optimistic concurrency, like actor appends, is owned by the
        // journal high-water rather than a snapshot-only aggregate sequence.
        state.sequence_nr = expected_sequence;
        if target_exists && expected_sequence == 0 && has_snapshot {
            state.last_snapshot_sequence_nr = 0;
            state.events_since_snapshot = 0;
        }
        if let Some(preflight_target) = preflight_target {
            let preflight_changed = preflight_target.target_existed != target_exists
                || (target_exists
                    && (preflight_target.state.sequence_nr != state.sequence_nr
                        || preflight_target.state.status != state.status
                        || preflight_target.state.fields != state.fields
                        || preflight_target.state.item_count != state.item_count
                        || preflight_target.state.counters != state.counters
                        || preflight_target.state.booleans != state.booleans
                        || preflight_target.state.lists != state.lists
                        || preflight_target.state.processed_idempotency_keys
                            != state.processed_idempotency_keys
                        || preflight_target.state.can_accept_event() != state.can_accept_event()));
            if preflight_changed {
                return Err(DispatchError::Conflict(format!(
                    "composite target {entity_type}:{entity_id} changed after authorization preflight; retry the composite action"
                )));
            }
        }
        let materialization_baseline =
            (target_exists && expected_sequence == 0 && has_snapshot).then(|| state.clone());
        if materialization_baseline.is_some() {
            state.record_internal_envelope();
        }
        let mut events = Vec::new();
        if !suppress_bootstrap_event
            && !target_exists
            && expected_sequence == 0
            && state.total_event_count == 0
        {
            let bootstrap = crate::entity_actor::EntityEvent {
                action: "Created".to_string(),
                from_status: String::new(),
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: serde_json::json!({}),
                idempotency_key: None,
            };
            events.push(composite_envelope(&persistence_id, &bootstrap)?);
            state.sequence_nr = state.sequence_nr.saturating_add(1);
            state.push_event_bounded(bootstrap);
        }
        streams.insert(
            persistence_id,
            AtomicCompositeStream {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                table: table.clone(),
                target_existed: target_exists,
                state,
                expected_sequence,
                snapshot_source,
                materialization_baseline,
                events,
            },
        );
        Ok(())
    }
}
