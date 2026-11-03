//! The database has one writer. Writes that arrive together take turns at
//! the store's write gate, where a write waits its turn, and not at the
//! database's write lock, where one that waits past its lock wait is refused
//! and its move is lost.

use std::time::{Duration, Instant};

use temper_runtime::persistence::EventStore;

use super::super::commit_probe::{CommitPoint, CommitProbe};
use super::{make_store, test_envelope};

/// How long a write waits for the database's write lock before its move is
/// refused (the store's default append attempt timeout).
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// One commit on a busy drive: the Factory's statements took 1.0 to 1.9 s on
/// its Tensorlake drive under load (Datadog, roll3, 26 Oct 2026).
const SLOW_COMMIT: Duration = Duration::from_secs(2);

/// Moves that arrive together: enough that the last one's turn comes after
/// [`LOCK_WAIT`] has passed.
const WRITES: u32 = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moves_that_arrive_together_on_a_slow_drive_all_land() {
    let store = make_store("write-queue")
        .await
        .with_commit_probe(CommitProbe::every(
            CommitPoint::Wrote,
            SLOW_COMMIT,
            LOCK_WAIT,
        ));
    let started = Instant::now();
    let moves: Vec<_> = (0..WRITES)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                let journal = format!("tenant-a:Job:job-{n}");
                let answer = store
                    .append(
                        &journal,
                        0,
                        &[test_envelope("Created", serde_json::json!({}))],
                    )
                    .await;
                (n, answer, started.elapsed())
            })
        })
        .collect();
    let mut refused = Vec::new();
    for answer in moves {
        let (n, answer, at) = answer.await.expect("move task");
        eprintln!("move {n}: {answer:?} after {} ms", at.as_millis());
        if let Err(error) = answer {
            refused.push(format!("move {n} after {} ms: {error}", at.as_millis()));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of {WRITES} moves were refused, though the database finished every \
         write within {:?}:\n{}",
        refused.len(),
        SLOW_COMMIT * WRITES,
        refused.join("\n")
    );
    for n in 0..WRITES {
        let events = store
            .read_events(&format!("tenant-a:Job:job-{n}"), 0)
            .await
            .expect("read");
        assert_eq!(events.len(), 1, "move {n} landed once");
    }
}

/// How long the first of two writes holds the gate in
/// [`a_write_that_holds_others_up_says_which_and_for_how_long`].
const HOLD: Duration = Duration::from_millis(300);

/// One `turso.write_gate held` log line: the write kind, how long it held the
/// gate and how that split between getting a connection and the transaction,
/// and how many writers were waiting when it let go.
#[derive(Debug, Default)]
struct HoldLine {
    operation: String,
    held_ms: u64,
    connection_ms: u64,
    transaction_ms: u64,
    waiting: u64,
}

/// Collects the `turso.write_gate held` lines logged while it is the default
/// subscriber.
#[derive(Clone, Default)]
struct HoldLines(std::sync::Arc<std::sync::Mutex<Vec<HoldLine>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for HoldLines {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut seen = Seen::default();
        event.record(&mut seen);
        if seen.is_hold {
            self.0.lock().expect("hold lines").push(seen.line);
        }
    }
}

/// One event's fields, and whether it is a `turso.write_gate held` line.
#[derive(Default)]
struct Seen {
    is_hold: bool,
    line: HoldLine,
}

impl tracing::field::Visit for Seen {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "operation" {
            self.line.operation = value.to_string();
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "held_ms" => self.line.held_ms = value,
            "connection_ms" => self.line.connection_ms = value,
            "transaction_ms" => self.line.transaction_ms = value,
            "waiting" => self.line.waiting = value,
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && format!("{value:?}") == "turso.write_gate held" {
            self.is_hold = true;
        }
    }
}

/// A write that keeps others waiting at the gate says so: which kind of write
/// it was, how long it held the gate, how much of that went to its transaction
/// rather than to getting a connection, and how many waited. In production
/// these lines carry the Temper's id, which the gate's metrics do not, so they
/// are how one Temper's queue is told from another's.
#[tokio::test]
async fn a_write_that_holds_others_up_says_which_and_for_how_long() {
    use tracing_subscriber::layer::SubscriberExt;

    let lines = HoldLines::default();
    let _logs =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(lines.clone()));
    let store = make_store("write-gate-holds")
        .await
        .with_commit_probe(CommitProbe::every(CommitPoint::Wrote, HOLD, LOCK_WAIT));
    let append = |n: u32| {
        let store = store.clone();
        async move {
            store
                .append(
                    &format!("tenant-a:Job:job-{n}"),
                    0,
                    &[test_envelope("Created", serde_json::json!({}))],
                )
                .await
                .expect("append")
        }
    };
    tokio::join!(append(0), append(1));

    let lines = lines.0.lock().expect("hold lines");
    let held_up = lines
        .iter()
        .find(|line| line.operation == "turso.append")
        .unwrap_or_else(|| {
            panic!("the first append, which kept the second waiting, says so: {lines:?}")
        });
    assert!(
        held_up.held_ms >= HOLD.as_millis() as u64 && held_up.waiting >= 1,
        "the line gives the hold and the writers it kept waiting: {held_up:?}"
    );
    // The commit was slow and the connection was not: the line puts the hold
    // in the transaction.
    assert!(
        held_up.transaction_ms >= HOLD.as_millis() as u64
            && held_up.connection_ms < HOLD.as_millis() as u64
            && held_up.connection_ms + held_up.transaction_ms == held_up.held_ms,
        "the line splits the hold into connection and transaction: {held_up:?}"
    );
    assert!(
        lines.iter().all(|line| line.waiting >= 1),
        "a hold that kept no one waiting is not logged: {lines:?}"
    );
}
