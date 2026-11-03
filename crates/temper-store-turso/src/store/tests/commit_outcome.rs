//! An append's answer is what its commit did, whatever stops or delays it.
//!
//! Two sweeps over the append path's fault points, for a single event and for
//! an event with the rows it commits:
//!
//! - an attempt timeout fires at each point (the probe pauses the append there
//!   past the timeout): the answer must agree with the journal, a success for
//!   an append that landed and an error for one that did not;
//! - the caller stops waiting after each step of the append: afterwards no
//!   connection holds the write lock, and the append is all or nothing (its
//!   event, its segment's end and its rows land together or not at all).

use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use temper_runtime::persistence::{CommitRows, EventStore, ScheduledActionRecord};

use super::super::TursoEventStore;
use super::super::commit_probe::{CommitPoint, CommitProbe};
use super::{make_store, test_envelope};

const JOURNAL: &str = "tenant-a:Job:job-1";

fn armed_row() -> ScheduledActionRecord {
    ScheduledActionRecord {
        tenant: "tenant-a".to_string(),
        entity_type: "Job".to_string(),
        entity_id: "job-1".to_string(),
        arm_id: "2.0".to_string(),
        action: "Fire".to_string(),
        due_at: "2026-01-01T00:10:00+00:00".to_string(),
        armed_by: "{}".to_string(),
    }
}

/// The two shapes of append: a lone event, and an event with its rows.
#[derive(Debug, Clone, Copy)]
enum Shape {
    Lone,
    WithRows,
}

const SHAPES: [Shape; 2] = [Shape::Lone, Shape::WithRows];

/// Append the record's second event in `shape`.
fn second_append(
    store: &TursoEventStore,
    shape: Shape,
) -> impl Future<Output = Result<u64, temper_runtime::persistence::PersistenceError>> + '_ {
    let armed = vec![armed_row()];
    async move {
        let event = [test_envelope("Start", serde_json::json!({}))];
        match shape {
            Shape::Lone => store.append(JOURNAL, 1, &event).await,
            Shape::WithRows => {
                store
                    .append_with_rows(
                        JOURNAL,
                        1,
                        &event,
                        CommitRows {
                            scheduled_actions: &armed,
                            ..CommitRows::default()
                        },
                    )
                    .await
            }
        }
    }
}

/// What the store holds for the record: its newest sequence, its open
/// segment's end, and whether the second event's row is there.
async fn stored(store: &TursoEventStore) -> (u64, Option<i64>, bool) {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
             WHERE tenant = 'tenant-a' AND entity_type = 'Job' AND entity_id = 'job-1'",
            (),
        )
        .await
        .expect("read sequence");
    let sequence = rows
        .next()
        .await
        .expect("row")
        .map(|row| row.get::<i64>(0).expect("sequence") as u64)
        .unwrap_or(0);
    drop(rows);
    let mut rows = conn
        .query(
            "SELECT MAX(end_sequence_nr) FROM event_segments \
             WHERE tenant = 'tenant-a' AND entity_type = 'Job' AND entity_id = 'job-1'",
            (),
        )
        .await
        .expect("read segments");
    let segment_end = rows
        .next()
        .await
        .expect("row")
        .and_then(|row| row.get::<Option<i64>>(0).expect("end"));
    drop(rows);
    let row = store
        .list_scheduled_actions("tenant-a")
        .await
        .expect("rows list")
        .iter()
        .any(|row| row.arm_id == "2.0");
    (sequence, segment_end, row)
}

/// A store holding the record's first event.
async fn store_with_first_event(name: &str) -> TursoEventStore {
    let store = make_store(name).await;
    store
        .append(
            JOURNAL,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("first event");
    store
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_attempt_timeout_at_any_point_answers_what_the_commit_did() {
    let mut wrong = Vec::new();
    for shape in SHAPES {
        for point in CommitPoint::ALL {
            let probe = CommitProbe::new(point, Duration::from_secs(2), Duration::from_millis(500));
            let store = store_with_first_event("timeout-sweep")
                .await
                .with_commit_probe(probe.clone());
            let answer = second_append(&store, shape).await;
            // Let a write that outlived its answer finish.
            tokio::time::sleep(Duration::from_secs(5)).await;
            let (sequence, segment_end, row) = stored(&store).await;
            let landed = sequence == 2;
            assert!(probe.fired(), "{shape:?} never reached {point:?}");
            let agrees = match &answer {
                Ok(seq) => landed && *seq == 2,
                Err(_) => !landed,
            };
            if !agrees {
                wrong.push(format!(
                    "{shape:?} paused at {point:?}: answered {answer:?}, journal at {sequence}"
                ));
            }
            if landed && segment_end != Some(2) {
                wrong.push(format!(
                    "{shape:?} paused at {point:?}: event landed, segment ends at {segment_end:?}"
                ));
            }
            if matches!(shape, Shape::WithRows) && landed != row {
                wrong.push(format!(
                    "{shape:?} paused at {point:?}: event landed {landed}, its row {row}"
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "an answer disagreed with the journal:\n{}",
        wrong.join("\n")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_caller_that_stops_waiting_leaves_all_or_nothing_and_no_lock() {
    let mut wrong = Vec::new();
    for shape in SHAPES {
        let mut polls = 0;
        loop {
            let store = store_with_first_event("drop-sweep").await;
            let mut append = Box::pin(second_append(&store, shape));
            let mut finished = false;
            for _ in 0..polls {
                let step = std::future::poll_fn(|cx| Poll::Ready(append.as_mut().poll(cx))).await;
                if step.is_ready() {
                    finished = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            drop(append);
            // Anything the append left running finishes now.
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Neither the gate nor the database's write lock is still held.
            let conn = store
                .write_connection("test", super::super::WritePriority::High)
                .await
                .expect("connection");
            let lock = tokio::time::timeout(Duration::from_secs(3), async {
                let tx = conn.begin_immediate().await?;
                tx.execute("CREATE TABLE IF NOT EXISTS lock_probe (x INTEGER)", ())
                    .await?;
                tx.commit().await
            })
            .await;
            match lock {
                Ok(Ok(())) => {}
                Ok(Err(error)) => wrong.push(format!(
                    "{shape:?} dropped after {polls} polls: next write failed: {error}"
                )),
                Err(_) => wrong.push(format!(
                    "{shape:?} dropped after {polls} polls: the write lock is still held"
                )),
            }
            drop(conn);

            // All or nothing.
            let (sequence, segment_end, row) = stored(&store).await;
            let landed = sequence == 2;
            if landed && segment_end != Some(2) {
                wrong.push(format!(
                    "{shape:?} dropped after {polls} polls: event landed, segment ends at {segment_end:?}"
                ));
            }
            if matches!(shape, Shape::WithRows) && landed != row {
                wrong.push(format!(
                    "{shape:?} dropped after {polls} polls: event landed {landed}, its row {row}"
                ));
            }
            if finished || polls > 400 {
                break;
            }
            polls += 1;
        }
    }
    assert!(
        wrong.is_empty(),
        "a dropped append left a partial write or a held lock:\n{}",
        wrong.join("\n")
    );
}
