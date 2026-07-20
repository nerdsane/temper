//! Atomic composite stream loading and bootstrap metadata.

use super::*;

impl crate::state::ServerState {
    pub(super) async fn ensure_atomic_composite_stream(
        &self,
        streams: &mut BTreeMap<String, AtomicCompositeStream>,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        preflight_target: Option<&PreflightCompositeTarget>,
        suppress_bootstrap_event: bool,
    ) -> Result<(), DispatchError> {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        if streams.contains_key(&persistence_id) {
            return Ok(());
        }

        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let (target_exists, mut state) = if let Some(target) = preflight_target {
            (target.target_existed, target.state.clone())
        } else {
            let target_exists = self
                .ensure_entity_loaded(tenant, entity_type, entity_id)
                .await;
            let state = if target_exists {
                self.get_tenant_entity_state(tenant, entity_type, entity_id)
                    .await
                    .map_err(DispatchError::Internal)?
                    .state
            } else {
                synthetic_initial_state(entity_type, entity_id, &table)
            };
            (target_exists, state)
        };
        let expected_sequence = state.sequence_nr;
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
            let event_version = state
                .sequence_nr
                .checked_add(1)
                .expect("composite bootstrap sequence overflow");
            let (envelope, clock) =
                composite_envelope(&persistence_id, &table, &state, &bootstrap, event_version)?;
            events.push(envelope);
            state.sequence_nr = event_version;
            apply_state_timeout_clock(&mut state, clock);
            state.push_event_bounded(bootstrap);
        }
        streams.insert(
            persistence_id,
            AtomicCompositeStream {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                target_existed: target_exists,
                state,
                expected_sequence,
                events,
            },
        );
        Ok(())
    }

    pub(super) async fn composite_event_already_persisted(
        &self,
        store: &crate::storage::BoxedEventStore,
        parent_persistence_id: &str,
        parent_idempotency: &str,
    ) -> Result<bool, DispatchError> {
        let envelopes = store
            .read_events(parent_persistence_id, 0)
            .await
            .map_err(|e| {
                DispatchError::Internal(format!(
                    "failed to read parent journal before CompositeEvent append: {e}"
                ))
            })?;
        Ok(envelopes.iter().any(|env| {
            env.event_type == COMPOSITE_EVENT_TYPE
                && serde_json::from_value::<CompositeEvent>(env.payload.clone())
                    .is_ok_and(|event| event.composite_idempotency_key == parent_idempotency)
        }))
    }

    pub(super) fn composite_batch_field_sync_mode(
        &self,
        tenant: &TenantId,
        backend: BackendLabel,
    ) -> FieldSyncMode {
        match backend {
            BackendLabel::Turso | BackendLabel::TursoRouted => FieldSyncMode::blob_refs_default(),
            _ if self.blob_store_for_tenant(tenant).is_ok() => FieldSyncMode::blob_refs_default(),
            _ => FieldSyncMode::InlineTruncate,
        }
    }
}
