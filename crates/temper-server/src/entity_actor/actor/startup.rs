//! Actor startup state construction and durable hydration.

use super::*;

impl EntityActor {
    pub(super) async fn pre_start_state(&self) -> Result<EntityState, ActorError> {
        // Snapshot the table for consistent startup (initial state + replay).
        // This is a cheap clone — TransitionTable is a few Vecs of strings.
        let table = self.table.read().expect("table lock poisoned").clone();

        let mut state = Self::build_initial_state(
            &self.entity_type,
            &self.entity_id,
            &table,
            &self.initial_fields,
        );

        // Replay events from Postgres to rebuild state (if persistence is configured).
        // Re-evaluates each event through the TransitionTable to reconstruct
        // all state variables (status, counters, booleans) — not just item_count.
        let loaded_snapshot = if let (Some(store), Some(backend)) =
            (self.event_journal.as_ref(), self.event_backend)
        {
            Self::replay_events(
                &table,
                store,
                backend,
                &mut state,
                &self.tenant,
                self.blob_store.as_ref(),
                true,
            )
            .await?
        } else {
            None
        };

        // A legacy snapshot may predate the dedicated timeout anchor and have
        // no replay tail from which to reconstruct it. Establish one
        // conservative budget at hydration so every snapshot written by the
        // current process persists the repair; repeated restarts must not keep
        // refreshing the fallback budget.
        if state.sequence_nr > 0
            && (state.state_timeout_clock_reset_at.is_none()
                || state.state_timeout_clock_reset_version.is_none())
            && table
                .state_timeouts
                .iter()
                .any(|timeout| timeout.state == state.status)
        {
            let repair_snapshot_sequence = state.last_snapshot_sequence_nr;
            if repair_snapshot_sequence == 0 {
                return Err(ActorError::custom(format!(
                    "cannot persist legacy timeout-anchor repair for {}:{} without a snapshot boundary",
                    self.entity_type, self.entity_id
                )));
            }
            let replayed_tail = state.sequence_nr.saturating_sub(repair_snapshot_sequence);
            let replayed_tail_count = usize::try_from(replayed_tail).map_err(|_| {
                ActorError::custom(format!(
                    "legacy timeout-anchor replay tail is too large for {}:{} ({replayed_tail} events)",
                    self.entity_type, self.entity_id
                ))
            })?;
            if state.state_timeout_clock_reset_at.is_none() {
                state.state_timeout_clock_reset_at = Some(sim_now());
            }
            state.state_timeout_clock_reset_version = Some(if state.sequence_nr != 0 {
                state.sequence_nr
            } else {
                u64::try_from(state.total_event_count).unwrap_or(u64::MAX)
            });

            // A missing anchor after replay proves that every post-snapshot
            // envelope was skipped without mutating domain state: any parsed
            // event updates or clears the anchor. Rewrite the loaded boundary
            // with boundary-consistent sequence metadata and leave those
            // skipped envelopes in the replay tail for the next restart.
            let mut repair_snapshot_state = state.clone();
            repair_snapshot_state.sequence_nr = repair_snapshot_sequence;
            repair_snapshot_state.last_snapshot_sequence_nr = repair_snapshot_sequence;
            repair_snapshot_state.events_since_snapshot = 0;
            let snapshot =
                Self::serialize_snapshot_state(&repair_snapshot_state).map_err(|error| {
                    ActorError::custom(format!(
                        "failed to encode legacy timeout-anchor repair for {}:{}: {error}",
                        self.entity_type, self.entity_id
                    ))
                })?;
            let Some(store) = self.event_journal.as_ref() else {
                return Err(ActorError::custom(format!(
                    "cannot persist legacy timeout-anchor repair for {}:{} without an event journal",
                    self.entity_type, self.entity_id
                )));
            };
            let persistence_id = self.persistence_id();
            let Some((loaded_snapshot_sequence, expected_snapshot)) = loaded_snapshot.as_ref()
            else {
                return Err(ActorError::custom(format!(
                    "cannot replace legacy timeout-anchor snapshot for {}:{} without the loaded boundary payload",
                    self.entity_type, self.entity_id
                )));
            };
            if *loaded_snapshot_sequence != repair_snapshot_sequence {
                return Err(ActorError::custom(format!(
                    "legacy timeout-anchor boundary changed for {}:{} (loaded {}, repairing {})",
                    self.entity_type,
                    self.entity_id,
                    loaded_snapshot_sequence,
                    repair_snapshot_sequence
                )));
            }
            store
                .replace_snapshot(
                    &persistence_id,
                    repair_snapshot_sequence,
                    expected_snapshot,
                    &snapshot,
                )
                .await
                .map_err(|error| {
                    ActorError::custom(format!(
                        "failed to persist legacy timeout-anchor repair for {}:{}: {error}",
                        self.entity_type, self.entity_id
                    ))
                })?;
            state.last_snapshot_sequence_nr = repair_snapshot_sequence;
            state.events_since_snapshot = replayed_tail_count;
            tracing::warn!(
                entity = %state.entity_id,
                state = %state.status,
                repair_snapshot_sequence,
                replayed_tail,
                "durably repaired missing legacy snapshot state-timeout clock anchor"
            );
        }

        // Persist a bootstrap Created event for first-time entities so initial
        // fields are durable and replayable.
        if self.event_journal.is_some() && state.sequence_nr == 0 {
            let created = EntityEvent {
                action: "Created".to_string(),
                from_status: String::new(),
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: self.initial_fields.clone(),
                idempotency_key: None,
            };

            if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            {
                let (_, clock) = self
                    .persist_event(
                        store,
                        backend,
                        &self.persistence_id(),
                        &table,
                        &mut state,
                        &created,
                    )
                    .await
                    .map_err(|e| {
                        ActorError::custom(format!(
                            "failed to persist bootstrap Created event for {}:{}: {}",
                            self.entity_type, self.entity_id, e
                        ))
                    })?;
                apply_state_timeout_clock(&mut state, clock);
            } else {
                Self::update_state_timeout_clock(&table, &mut state, &created);
            }
            state.push_event_bounded(created);
        }

        Ok(state)
    }
}
