//! Removing several entities together: all of them, or none.
//!
//! A delete that cascades removes the requested entity and the rows that
//! belong to it, each as it was read and checked. With an event journal their
//! tombstones are appended in one `append_batch`, each conditional on the
//! journal sequence its state was read at, so either every one is removed or,
//! when any changed after it was read, none is. No reader of the journal sees
//! part of the set removed, so the set needs no removal order, and a cycle of
//! references inside it (a row that references itself, two rows that
//! reference each other) needs none either. After the write each entity's
//! actor is stopped, so the next read replays the tombstone, and its query
//! projection row and any armed timer are removed. A projection removal that
//! fails leaves a row whose journal has moved past it, which the delete
//! reference lookup does not trust (`delete_references`).
//!
//! A server with no event journal refuses a delete that removes several
//! entities, before it writes anything (`BatchDeleteError::NoJournal`).
//!
//! The work runs in its own task, so a request that goes away part way (its
//! client disconnected) cannot leave live actors serving the state from before
//! a committed tombstone.

use temper_runtime::persistence::{
    AppendProjection, EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use tracing::{Instrument, instrument};

use super::ServerState;
use crate::entity_actor::{EntityEvent, EntityState};
use crate::storage::BoxedEventStore;

/// Why a batch delete removed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchDeleteError {
    /// An entity changed after its state was read.
    Changed(String),
    /// The server keeps no event journal, so it has no write that removes
    /// several entities as one change.
    NoJournal,
    /// The store could not take the delete.
    Storage(String),
}

impl ServerState {
    /// Remove every entity in `members` together: only while each one is
    /// still in the state given for it, and then all of them. Each member is
    /// a distinct, live entity of `tenant`.
    #[instrument(skip_all, fields(otel.name = "entity.delete_entities_together", tenant = %tenant, members = members.len()))]
    pub(crate) async fn delete_entities_together(
        &self,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        debug_assert!(
            {
                let mut ids = std::collections::BTreeSet::new();
                members
                    .iter()
                    .all(|member| ids.insert((&member.entity_type, &member.entity_id)))
            },
            "batch delete members must be distinct"
        );
        // A store with no atomic multi-journal write refuses a delete that
        // removes several entities. Without a journal each entity lives only
        // in its own actor, and no single write covers several actors, so
        // they would be deleted one at a time; a delete that stopped part way
        // would leave the rest pointing at entities that are gone. Every
        // journal store's `append_batch` commits all of its appends or none.
        let Some((store, _backend)) = self.event_journal() else {
            return Err(BatchDeleteError::NoJournal);
        };
        let state = self.clone();
        let tenant = tenant.clone();
        // determinism-ok: the task only outlives a request that goes away; it
        // is awaited here, so the delete's order of effects does not change.
        tokio::spawn(
            async move {
                state
                    .append_tombstones_together(&store, &tenant, members)
                    .await
            }
            .instrument(tracing::Span::current()),
        )
        .await
        .map_err(|error| BatchDeleteError::Storage(format!("delete task failed: {error}")))?
    }

    /// One `append_batch` of every member's tombstone, each conditional on the
    /// sequence its state was read at.
    async fn append_tombstones_together(
        &self,
        store: &BoxedEventStore,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        let mut appends = Vec::with_capacity(members.len());
        let mut deleted = Vec::with_capacity(members.len());
        // Each record's query-plane row goes in the batch's commit, as in a
        // single delete's.
        let commits_rows = store.commits_entity_rows();
        for mut member in members {
            debug_assert!(member.sequence_nr > 0, "a journaled entity has events");
            let persistence_id = format!("{tenant}:{}:{}", member.entity_type, member.entity_id);
            let tombstone = EntityEvent::tombstone(member.status.clone());
            let payload = crate::entity_actor::bootstrap::event_payload(&tombstone, &member)
                .map_err(|error| BatchDeleteError::Storage(error.to_string()))?;
            appends.push(PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: member.sequence_nr,
                events: vec![PersistenceEnvelope {
                    sequence_nr: member.sequence_nr + 1,
                    event_type: tombstone.action.clone(),
                    payload,
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: tombstone.timestamp,
                        actor_id: persistence_id,
                    },
                }],
                state_timer: commits_rows
                    .then_some(temper_runtime::persistence::AppendStateTimer::End),
                projection: commits_rows.then_some(AppendProjection::Remove),
            });
            member.record_tombstone(tombstone);
            member.sequence_nr += 1;
            deleted.push(member);
        }

        match store.append_batch(&appends).await {
            Ok(results) => debug_assert_eq!(results.len(), deleted.len()),
            Err(PersistenceError::ConcurrencyViolation { expected, actual }) => {
                // The conflicting writer may live on another server, so our
                // actors may still hold exactly the states this batch read.
                // The store does not name the conflicting member: reload them
                // all before the route plans and authorizes the delete again.
                // Actor refresh does not delete entities. Keep every id
                // discoverable throughout recovery, including if a reload
                // fails or another request lists a fully hydrated type.
                for member in &deleted {
                    self.stop_entity_actor(tenant, &member.entity_type, &member.entity_id);
                }
                for member in &deleted {
                    let current = self
                        .get_tenant_entity_state(tenant, &member.entity_type, &member.entity_id)
                        .await
                        .map_err(|error| {
                            BatchDeleteError::Storage(format!(
                                "delete conflict recovery failed for {}('{}'): {error}",
                                member.entity_type, member.entity_id
                            ))
                        })?;
                    self.cache_entity_status(tenant, &current.state);
                }
                return Err(BatchDeleteError::Changed(format!(
                    "an entity it removes changed after it was read (journal at {actual}, read at {expected})"
                )));
            }
            Err(error) => return Err(BatchDeleteError::Storage(error.to_string())),
        }

        // The tombstones are the journal's truth from here on. Each live
        // actor still holds the state from before them, so it goes first.
        for member in &deleted {
            self.cache_entity_status(tenant, member);
            self.stop_and_remove_entity(tenant, &member.entity_type, &member.entity_id);
        }
        for member in &deleted {
            // A deleted record keeps no timer, as a single delete does.
            self.reconcile_state_timers(
                tenant,
                &member.entity_type,
                &member.entity_id,
                member,
                None,
            )
            .await;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "batch_delete_test.rs"]
mod tests;
