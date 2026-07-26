//! Bounded entity recovery from snapshots and durable journals.

use super::*;

mod envelope;
mod stable_source;

use envelope::apply_replayed_envelope;
pub(crate) use stable_source::{
    CapturedEntitySnapshot, StableEntitySource, recover_entity_state_from_stable_sources,
    stable_entity_source_is_current,
};

const JOURNAL_REPLAY_PAGE_SIZE: usize = 1_024;
const MAX_STABLE_RECOVERY_ATTEMPTS: usize = 3;

/// Immutable dependencies and identity shared by entity recovery strategies.
#[derive(Clone, Copy)]
pub(crate) struct EntityRecoveryContext<'a> {
    /// Tenant that owns the durable entity.
    pub(crate) tenant: &'a str,
    /// Durable entity type.
    pub(crate) entity_type: &'a str,
    /// Durable entity identifier.
    pub(crate) entity_id: &'a str,
    /// Transition table used to replay domain events.
    pub(crate) table: &'a TransitionTable,
    /// Event store that owns the durable sources.
    pub(crate) store: &'a BoxedEventStore,
    /// Backend label used for replay field synchronization and diagnostics.
    pub(crate) backend: BackendLabel,
    /// Initial fields used before applying durable state.
    pub(crate) initial_fields: &'a serde_json::Value,
    /// Optional overflow-blob store used while replaying fields.
    pub(crate) blob_store: Option<&'a crate::blob_store::BlobStore>,
}

#[derive(Clone, Copy)]
struct ReplayPolicy {
    strict_journal_read: bool,
    load_snapshot: bool,
    strict_event_decode: bool,
    replay_full_journal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedReplaySource {
    journal_boundary: JournalBoundary,
    snapshot: Option<CapturedEntitySnapshot>,
}

/// Actor state and the exact snapshot generation that participated in recovery.
pub(crate) struct RecoveredEntityState {
    /// State reconstructed from one closed durable source generation.
    pub(crate) state: EntityState,
    /// Exact snapshot source the next derived append must validate atomically.
    pub(crate) snapshot_source: SnapshotSourceFence,
}

fn validate_and_normalize_snapshot_state(
    state: &mut EntityState,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    sequence_nr: u64,
) -> Result<(), ActorError> {
    if state.entity_type != entity_type || state.entity_id != entity_id {
        return Err(ActorError::custom(format!(
            "durable snapshot identity mismatch for {entity_type}:{entity_id} at sequence {sequence_nr}"
        )));
    }
    // `Deleted` is the runtime's terminal lifecycle marker rather than a
    // user-declared IOA state. Snapshot writers may persist that terminal state,
    // so validate every other status against the transition table while retaining
    // the canonical deletion marker for tombstone/source-precedence recovery.
    if state.status != "Deleted" && !table.states.contains(&state.status) {
        return Err(ActorError::custom(format!(
            "durable snapshot has invalid status '{}' for {entity_type}:{entity_id} at sequence {sequence_nr}",
            state.status
        )));
    }
    let fields = state.fields.as_object_mut().ok_or_else(|| {
        ActorError::custom(format!(
            "durable snapshot fields are not an object for {entity_type}:{entity_id} at sequence {sequence_nr}"
        ))
    })?;
    fields.insert(
        "Id".to_string(),
        serde_json::Value::String(entity_id.to_string()),
    );
    fields.insert(
        "Status".to_string(),
        serde_json::Value::String(state.status.clone()),
    );
    Ok(())
}

async fn replay_events(
    context: EntityRecoveryContext<'_>,
    state: &mut EntityState,
    policy: ReplayPolicy,
    captured_source: Option<&CapturedReplaySource>,
) -> Result<(), ActorError> {
    let replay_start = Instant::now(); // determinism-ok: production replay metric only
    let persistence_id = format!(
        "{}:{}:{}",
        context.tenant, state.entity_type, state.entity_id
    );
    let mut effective_policy = policy;
    let mut from_sequence = 0;
    let mut loaded_snapshot = false;
    let mut snapshot_provenance = None;
    let journal_boundary = match captured_source {
        Some(source) => source.journal_boundary,
        None => context
            .store
            .journal_boundary(&persistence_id)
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to read durable journal boundary for {}:{}: {error}",
                    state.entity_type, state.entity_id
                ))
            })?,
    };

    if policy.load_snapshot {
        let snapshot_result = match captured_source {
            Some(source) => Ok(source
                .snapshot
                .as_ref()
                .map(|snapshot| (snapshot.sequence_nr, snapshot.state.clone()))),
            None => context.store.load_snapshot(&persistence_id).await,
        };
        match snapshot_result {
            Ok(Some((snapshot_sequence, snapshot_bytes))) => {
                if let Some(provenance) =
                    EntityActor::apply_snapshot_bytes(state, snapshot_sequence, &snapshot_bytes)
                {
                    validate_and_normalize_snapshot_state(
                        state,
                        context.entity_type,
                        context.entity_id,
                        context.table,
                        snapshot_sequence,
                    )?;
                    from_sequence = snapshot_sequence;
                    loaded_snapshot = true;
                    snapshot_provenance = Some(provenance);
                    tracing::info!(
                        entity = %state.entity_id,
                        seq = snapshot_sequence,
                        "loaded snapshot before replay"
                    );
                } else {
                    if captured_source.is_some() {
                        return Err(ActorError::custom(format!(
                            "incompatible durable snapshot schema for {}:{} at sequence {snapshot_sequence}",
                            state.entity_type, state.entity_id
                        )));
                    }
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = snapshot_sequence,
                        "failed to deserialize snapshot, falling back to full replay"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    entity = %state.entity_id,
                    error = %error,
                    "failed to load snapshot, falling back to full replay"
                );
            }
        }
    }

    let snapshot_hides_terminal = loaded_snapshot
        && state.status != "Deleted"
        && journal_boundary
            .first_terminal_sequence
            .is_some_and(|sequence| sequence <= from_sequence);
    let snapshot_lacks_journal_provenance = loaded_snapshot
        && journal_boundary.latest_sequence > 0
        && !matches!(
            snapshot_provenance,
            Some(
                SnapshotProvenance::Journal { through_sequence }
                    | SnapshotProvenance::LegacyJournal { through_sequence }
            )
                if through_sequence == from_sequence
                    && through_sequence <= journal_boundary.latest_sequence
        );
    if snapshot_hides_terminal || snapshot_lacks_journal_provenance {
        let mut snapshot_fields = state.fields.clone();
        if let Some(fields) = snapshot_fields.as_object_mut() {
            fields.insert(
                "Status".to_string(),
                serde_json::Value::String(context.table.initial_state.clone()),
            );
        }
        tracing::warn!(
            entity = %state.entity_id,
            snapshot_sequence = from_sequence,
            journal_sequence = journal_boundary.latest_sequence,
            snapshot_hides_terminal,
            snapshot_lacks_journal_provenance,
            "replaying journal from zero over an untrusted snapshot baseline"
        );
        *state = EntityActor::build_initial_state(
            context.entity_type,
            context.entity_id,
            context.table,
            &snapshot_fields,
        );
        from_sequence = 0;
        loaded_snapshot = false;
        effective_policy.strict_journal_read = true;
        effective_policy.strict_event_decode = true;
        effective_policy.replay_full_journal = true;
    }

    let replay_event_budget = journal_boundary
        .latest_sequence
        .saturating_sub(from_sequence);
    if !effective_policy.replay_full_journal
        && replay_event_budget > MAX_EVENTS_SINCE_SNAPSHOT as u64
    {
        return Err(ActorError::custom(format!(
            "snapshot tail replay budget exceeded for {}:{} ({} > {} events since snapshot)",
            state.entity_type, state.entity_id, replay_event_budget, MAX_EVENTS_SINCE_SNAPSHOT
        )));
    }

    if replay_event_budget == 0 && effective_policy.strict_journal_read {
        let probe = context
            .store
            .read_events_page(
                &persistence_id,
                journal_boundary.latest_sequence,
                journal_boundary.latest_sequence,
                1,
            )
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to verify journal readability for {}:{}: {error}",
                    state.entity_type, state.entity_id
                ))
            })?;
        if !probe.is_empty() {
            return Err(ActorError::custom(format!(
                "journal readability probe for {}:{} crossed durable high-water {}",
                state.entity_type, state.entity_id, journal_boundary.latest_sequence
            )));
        }
    }

    let mut cursor = from_sequence;
    let mut replayed_count = 0_u64;
    while cursor < journal_boundary.latest_sequence {
        let remaining = journal_boundary.latest_sequence - cursor;
        let page_len = usize::try_from(remaining.min(JOURNAL_REPLAY_PAGE_SIZE as u64))
            .expect("bounded journal page length fits usize");
        let page = match context
            .store
            .read_events_page(
                &persistence_id,
                cursor,
                journal_boundary.latest_sequence,
                page_len,
            )
            .await
        {
            Ok(page) => page,
            Err(error)
                if effective_policy.strict_journal_read
                    || journal_boundary.latest_sequence > from_sequence =>
            {
                return Err(ActorError::custom(format!(
                    "failed to read events for replay of {}:{}: {error}",
                    state.entity_type, state.entity_id
                )));
            }
            Err(error) => {
                tracing::error!(
                    entity = %state.entity_id,
                    error = %error,
                    "failed to read events for replay — starting fresh"
                );
                break;
            }
        };
        if page.len() != page_len {
            return Err(ActorError::custom(format!(
                "journal replay for {}:{} returned {} events where {} were required through durable high-water {}",
                state.entity_type,
                state.entity_id,
                page.len(),
                page_len,
                journal_boundary.latest_sequence
            )));
        }
        for (offset, envelope) in page.iter().enumerate() {
            let expected_sequence = cursor + offset as u64 + 1;
            if envelope.sequence_nr != expected_sequence {
                return Err(ActorError::custom(format!(
                    "journal replay for {}:{} expected sequence {}, received {}",
                    state.entity_type, state.entity_id, expected_sequence, envelope.sequence_nr
                )));
            }
            apply_replayed_envelope(
                context.table,
                context.backend,
                state,
                context.tenant,
                context.blob_store,
                envelope,
                effective_policy.strict_event_decode,
            )
            .await?;
        }
        cursor = page
            .last()
            .map(|event| event.sequence_nr)
            .expect("validated non-empty journal page");
        replayed_count = replayed_count.saturating_add(page.len() as u64);
    }

    if cursor < journal_boundary.latest_sequence {
        return Err(ActorError::custom(format!(
            "journal replay for {}:{} stopped at sequence {} below durable high-water {}",
            state.entity_type, state.entity_id, cursor, journal_boundary.latest_sequence
        )));
    }
    if journal_boundary.first_terminal_sequence.is_some()
        && (state.status != "Deleted" || state.sequence_nr < journal_boundary.latest_sequence)
    {
        return Err(ActorError::custom(format!(
            "journal replay for {}:{} did not preserve terminal history through sequence {}",
            state.entity_type, state.entity_id, journal_boundary.latest_sequence
        )));
    }

    if replayed_count > 0 {
        tracing::info!(
            entity = %state.entity_id,
            snapshot_loaded = loaded_snapshot,
            replayed = replayed_count,
            status = %state.status,
            seq = state.sequence_nr,
            total_events = state.total_event_count,
            events_since_snapshot = state.events_since_snapshot,
            recent_events = state.events.len(),
            counters = ?state.counters,
            booleans = ?state.booleans,
            "state rebuilt from bounded event-journal pages via TransitionTable"
        );
    } else if loaded_snapshot {
        tracing::info!(
            entity = %state.entity_id,
            seq = state.sequence_nr,
            total_events = state.total_event_count,
            events_since_snapshot = state.events_since_snapshot,
            "state restored from snapshot (no delta events)"
        );
    }
    crate::runtime_metrics::record_event_replay_duration(
        replay_start.elapsed(),
        context.tenant,
        &state.entity_type,
    );
    if snapshot_hides_terminal || snapshot_lacks_journal_provenance {
        // A legacy generation required one strict migration replay. Do not claim
        // a durable snapshot boundary, and retain the raw journal tail that must
        // stay within the next restart's bounded replay budget.
        state.last_snapshot_sequence_nr = 0;
        state.events_since_snapshot = usize::try_from(state.sequence_nr).unwrap_or(usize::MAX);
    }
    Ok(())
}

async fn capture_replay_source(
    context: EntityRecoveryContext<'_>,
    persistence_id: &str,
) -> Result<CapturedReplaySource, ActorError> {
    let journal_boundary = context
        .store
        .journal_boundary(persistence_id)
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to capture durable journal source for {}:{}: {error}",
                context.entity_type, context.entity_id
            ))
        })?;
    let snapshot = context
        .store
        .load_snapshot(persistence_id)
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to capture durable snapshot source for {}:{}: {error}",
                context.entity_type, context.entity_id
            ))
        })?
        .map(|(sequence_nr, state)| CapturedEntitySnapshot { sequence_nr, state });
    Ok(CapturedReplaySource {
        journal_boundary,
        snapshot,
    })
}

fn snapshot_source_fence(snapshot: &Option<CapturedEntitySnapshot>) -> SnapshotSourceFence {
    match snapshot {
        Some(snapshot) => SnapshotSourceFence::Exact {
            sequence_nr: snapshot.sequence_nr,
            state: snapshot.state.clone(),
        },
        None => SnapshotSourceFence::Absent,
    }
}

/// Rebuild actor state from one exact snapshot/journal generation and close both
/// sources before returning its append fence.
pub(crate) async fn recover_entity_state_with_source_from_store(
    context: EntityRecoveryContext<'_>,
    strict_journal_read: bool,
) -> Result<RecoveredEntityState, ActorError> {
    let persistence_id = format!(
        "{}:{}:{}",
        context.tenant, context.entity_type, context.entity_id
    );
    for _attempt in 1..=MAX_STABLE_RECOVERY_ATTEMPTS {
        let source = capture_replay_source(context, &persistence_id).await?;
        let mut state = EntityActor::build_initial_state(
            context.entity_type,
            context.entity_id,
            context.table,
            context.initial_fields,
        );
        replay_events(
            context,
            &mut state,
            ReplayPolicy {
                strict_journal_read,
                load_snapshot: true,
                // A stable byte range is not authoritative state if an event in
                // that range was silently skipped. Actor recovery fails closed so
                // the next append cannot certify derived rows from partial replay.
                strict_event_decode: true,
                replay_full_journal: false,
            },
            Some(&source),
        )
        .await?;
        let closed = capture_replay_source(context, &persistence_id).await?;
        if closed == source {
            // Snapshot sequence is an aggregate baseline, while optimistic
            // append concurrency is owned by the journal high-water. A
            // snapshot-only generation therefore accepts its first event at 1.
            state.sequence_nr = source.journal_boundary.latest_sequence;
            if state.sequence_nr == 0 && source.snapshot.is_some() {
                // The first journal event will materialize this complete baseline.
                // Reset replay-budget coordinates to that new journal generation;
                // retaining a migration snapshot's aggregate sequence could defer
                // snapshots beyond the fixed 10k event budget.
                state.last_snapshot_sequence_nr = 0;
                state.events_since_snapshot = 0;
            }
            return Ok(RecoveredEntityState {
                state,
                snapshot_source: snapshot_source_fence(&source.snapshot),
            });
        }
        tracing::warn!(
            entity = %context.entity_id,
            "durable source changed during actor recovery; retrying"
        );
    }
    Err(ActorError::custom(format!(
        "durable source generation for {}:{} did not stabilize after {MAX_STABLE_RECOVERY_ATTEMPTS} attempts",
        context.entity_type, context.entity_id
    )))
}

/// Rebuild an entity's current state from its snapshot plus bounded journal tail.
#[cfg(test)]
pub(crate) async fn recover_entity_state_from_store(
    context: EntityRecoveryContext<'_>,
    strict_journal_read: bool,
) -> Result<EntityState, ActorError> {
    Ok(
        recover_entity_state_with_source_from_store(context, strict_journal_read)
            .await?
            .state,
    )
}
