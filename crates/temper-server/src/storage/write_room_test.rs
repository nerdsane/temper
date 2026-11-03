//! A disk under the floor refuses every append before anything is written,
//! and writes go on by themselves once it has room. `/healthz` reads what the
//! appends did: refusing for room, failing until one lands, or writing.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use temper_runtime::persistence::{
    CommitRows, EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
};
use temper_store_sim::SimEventStore;

const JOURNAL: &str = "default:Order:o-1";

fn envelope(sequence_nr: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: "AddItem".into(),
        payload: serde_json::json!({ "action": "AddItem", "params": {} }),
        metadata: EventMetadata {
            event_id: temper_runtime::scheduler::sim_uuid(),
            causation_id: temper_runtime::scheduler::sim_uuid(),
            correlation_id: temper_runtime::scheduler::sim_uuid(),
            timestamp: temper_runtime::scheduler::sim_now(),
            actor_id: JOURNAL.into(),
        },
    }
}

/// A store on a disk whose available room the test sets.
fn store_on_disk(available: &Arc<AtomicU64>) -> (Arc<SimEventStore>, BoxedEventStore) {
    let store = Arc::new(SimEventStore::no_faults(7));
    let available = Arc::clone(available);
    let room = StorageRoom::read_with("/sim/disk", move |_| Ok(available.load(Ordering::SeqCst)));
    let journal = BoxedEventStore::from_arc(store.clone()).with_storage_room(room);
    (store, journal)
}

fn is_storage_full(error: &PersistenceError, available: u64) -> bool {
    matches!(
        error,
        PersistenceError::StorageFull { available_bytes, floor_bytes, .. }
            if *available_bytes == available && *floor_bytes == STORAGE_FLOOR_BYTES
    )
}

#[tokio::test]
async fn a_disk_under_the_floor_refuses_every_append_and_writes_go_on_once_it_has_room() {
    let short = STORAGE_FLOOR_BYTES - 1;
    let available = Arc::new(AtomicU64::new(short));
    let (store, journal) = store_on_disk(&available);
    let events = [envelope(1)];

    let refusals = [
        journal.append(JOURNAL, 0, &events).await.unwrap_err(),
        journal
            .append_with_keys(JOURNAL, 0, &events, &[])
            .await
            .unwrap_err(),
        journal
            .append_with_rows(JOURNAL, 0, &events, CommitRows::default())
            .await
            .unwrap_err(),
        journal
            .append_batch(&[PersistenceAppend {
                persistence_id: JOURNAL.into(),
                expected_sequence: 0,
                events: events.to_vec(),
                state_timer: None,
                projection: None,
            }])
            .await
            .unwrap_err(),
    ];
    for refusal in &refusals {
        assert!(is_storage_full(refusal, short), "{refusal}");
    }
    assert!(
        store.read_events(JOURNAL, 0).await.unwrap().is_empty(),
        "a refused append writes nothing"
    );
    assert!(
        matches!(journal.write_health(), WriteHealth::Refusing(_)),
        "{:?}",
        journal.write_health()
    );

    available.store(STORAGE_FLOOR_BYTES, Ordering::SeqCst);
    assert_eq!(
        journal.write_health(),
        WriteHealth::Writing,
        "the refusal ends once the disk has room, before the next append"
    );
    assert_eq!(journal.append(JOURNAL, 0, &events).await.unwrap(), 1);
    assert_eq!(store.read_events(JOURNAL, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_append_makes_writes_fail_until_one_lands_on_any_clone() {
    let store = Arc::new(SimEventStore::no_faults(9));
    let journal = BoxedEventStore::from_arc(store.clone());
    let reader = journal.clone();

    store.fail_next_appends(JOURNAL, 1);
    journal
        .append(JOURNAL, 0, &[envelope(1)])
        .await
        .unwrap_err();
    assert!(
        matches!(reader.write_health(), WriteHealth::Failing(_)),
        "{:?}",
        reader.write_health()
    );

    // A conflict says nothing about whether the store takes writes.
    store.inject_concurrency_violations(JOURNAL, 1);
    let conflict = journal
        .append(JOURNAL, 0, &[envelope(1)])
        .await
        .unwrap_err();
    assert!(
        matches!(conflict, PersistenceError::ConcurrencyViolation { .. }),
        "{conflict}"
    );
    assert!(matches!(reader.write_health(), WriteHealth::Failing(_)));

    journal.append(JOURNAL, 0, &[envelope(1)]).await.unwrap();
    assert_eq!(reader.write_health(), WriteHealth::Writing);
}

#[tokio::test]
async fn a_store_that_failed_stays_failing_through_a_refusal_for_room() {
    let available = Arc::new(AtomicU64::new(STORAGE_FLOOR_BYTES));
    let (store, journal) = store_on_disk(&available);
    store.fail_next_appends(JOURNAL, 1);
    journal
        .append(JOURNAL, 0, &[envelope(1)])
        .await
        .unwrap_err();

    available.store(STORAGE_FLOOR_BYTES - 1, Ordering::SeqCst);
    let refused = journal
        .append(JOURNAL, 0, &[envelope(1)])
        .await
        .unwrap_err();
    assert!(
        is_storage_full(&refused, STORAGE_FLOOR_BYTES - 1),
        "{refused}"
    );
    available.store(STORAGE_FLOOR_BYTES, Ordering::SeqCst);
    assert!(
        matches!(journal.write_health(), WriteHealth::Failing(_)),
        "room again is not a landed append: {:?}",
        journal.write_health()
    );

    journal.append(JOURNAL, 0, &[envelope(1)]).await.unwrap();
    assert_eq!(journal.write_health(), WriteHealth::Writing);
}

#[tokio::test]
async fn a_disk_that_cannot_be_read_refuses_nothing() {
    let store = Arc::new(SimEventStore::no_faults(11));
    let room = StorageRoom::read_with("/sim/gone", |_| {
        Err(io::Error::new(io::ErrorKind::NotFound, "no such disk"))
    });
    let journal = BoxedEventStore::from_arc(store).with_storage_room(room);
    assert_eq!(journal.append(JOURNAL, 0, &[envelope(1)]).await.unwrap(), 1);
    assert_eq!(journal.write_health(), WriteHealth::Writing);
}

/// A trajectory sink that counts the rows it is handed.
#[derive(Default)]
struct CountingTrajectories(AtomicU64);

#[async_trait::async_trait]
impl TrajectorySink for CountingTrajectories {
    async fn persist_trajectory_entry(&self, _entry: &TrajectoryEntry) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn a_disk_under_the_floor_writes_no_trajectory_row_until_it_has_room() {
    let available = Arc::new(AtomicU64::new(STORAGE_FLOOR_BYTES - 1));
    let rows = Arc::new(CountingTrajectories::default());
    let room_of = Arc::clone(&available);
    let sink = RoomCheckedTrajectories {
        inner: rows.clone(),
        room: StorageRoom::read_with("/sim/disk", move |_| Ok(room_of.load(Ordering::SeqCst))),
    };
    let entry: TrajectoryEntry = serde_json::from_value(serde_json::json!({
        "timestamp": "2026-10-26T00:00:00Z", "tenant": "default", "entity_type": "Order",
        "entity_id": "o-1", "action": "AddItem", "success": false,
    }))
    .expect("a trajectory entry");

    let refused = sink.persist_trajectory_entry(&entry).await.unwrap_err();
    assert!(refused.starts_with("storage full"), "{refused}");
    assert_eq!(
        rows.0.load(Ordering::SeqCst),
        0,
        "a refused row is not written"
    );

    available.store(STORAGE_FLOOR_BYTES, Ordering::SeqCst);
    sink.persist_trajectory_entry(&entry).await.unwrap();
    assert_eq!(
        rows.0.load(Ordering::SeqCst),
        1,
        "rows are written once the disk has room"
    );
}

#[test]
fn the_disk_this_test_runs_on_reads_its_room() {
    let available = available_bytes(Path::new(".")).expect("statvfs reads the working directory");
    assert!(available > 0, "a disk with no room left cannot run tests");
}
