//! Trajectory rows go to storage in batches: one transaction, so one turn
//! of the write lock, for every row waiting when the batch begins.
//!
//! Each row used to be an insert of its own, behind a write permit and with a
//! one-second timeout: while moves held the lock, rows timed out and were
//! lost (37 of them on 25 Oct). Now a row joins the waiting batch and its
//! caller awaits that batch's answer. One drain at a time writes the rows
//! waiting, on a task of its own, so a caller that stops waiting loses
//! nothing; no timeout cuts a write short. A transient failure is retried
//! with the rows still in hand; any other failure is each row's answer.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use temper_runtime::persistence::{PersistenceError, storage_error};
use tokio::sync::oneshot;

use super::TursoEventStore;
use super::write_gate::WritePriority;
use crate::TursoTrajectoryInsert;
use crate::driver::{Value, params_from_iter};
use crate::retry::retry_persistence_with_max_attempts;

/// Rows per `INSERT`: 20 columns each, well under SQLite's 32,766 bound
/// on parameters per statement.
const ROWS_PER_INSERT: usize = 200;

/// Rows per transaction. A drain takes at most this many, so one batch
/// holds the write lock for a bounded time and the moves waiting behind it
/// get their turn.
const ROWS_PER_BATCH: usize = 1_000;

/// Attempts for a batch whose write failed with a transient error.
const BATCH_ATTEMPTS: usize = 5;

/// The rows waiting for a drain, and whether one is running.
#[derive(Debug, Default)]
pub(super) struct TrajectoryBatch {
    waiting: Mutex<Vec<Waiting>>,
    draining: AtomicBool,
}

#[derive(Debug)]
struct Waiting {
    row: TrajectoryRow,
    written: oneshot::Sender<Result<(), String>>,
}

/// One trajectory row, owned.
#[derive(Debug)]
struct TrajectoryRow([Value; 20]);

impl TrajectoryRow {
    fn new(entry: &TursoTrajectoryInsert<'_>) -> Self {
        let text = |value: Option<&str>| value.map_or(Value::Null, |v| Value::from(v.to_string()));
        let flag = |value: Option<bool>| value.map_or(Value::Null, |v| Value::from(i64::from(v)));
        Self([
            Value::from(entry.tenant.to_string()),
            Value::from(entry.entity_type.to_string()),
            Value::from(entry.entity_id.to_string()),
            Value::from(entry.action.to_string()),
            Value::from(i64::from(entry.success)),
            text(entry.from_status),
            text(entry.to_status),
            text(entry.error),
            text(entry.agent_id),
            text(entry.session_id),
            flag(entry.authz_denied),
            text(entry.denied_resource),
            text(entry.denied_module),
            text(entry.source),
            flag(entry.spec_governed),
            Value::from(entry.created_at.to_string()),
            text(entry.request_body),
            text(entry.intent),
            text(entry.matched_policy_ids),
            entry.capture_seq.map_or(Value::Null, Value::from),
        ])
    }
}

impl TrajectoryBatch {
    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<Waiting>> {
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take the next batch, or end the drain when nothing waits. None: the
    /// drain ended.
    fn next_batch(&self) -> Option<Vec<Waiting>> {
        loop {
            {
                let mut waiting = self.waiting();
                if !waiting.is_empty() {
                    let take = waiting.len().min(ROWS_PER_BATCH);
                    return Some(waiting.drain(..take).collect());
                }
            }
            self.draining.store(false, Ordering::Release);
            // A row that joined after the check above found a drain running
            // and started none: take it up again, unless another drain has.
            if self.waiting().is_empty() || self.draining.swap(true, Ordering::AcqRel) {
                return None;
            }
        }
    }
}

impl TursoEventStore {
    /// Write `entry` with the rows waiting beside it; its answer is its
    /// batch's.
    pub(super) async fn persist_trajectory_batched(
        &self,
        entry: &TursoTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let (written, answer) = oneshot::channel();
        self.trajectory_batch.waiting().push(Waiting {
            row: TrajectoryRow::new(entry),
            written,
        });
        if !self.trajectory_batch.draining.swap(true, Ordering::AcqRel) {
            let store = self.clone();
            tokio::spawn(async move { store.drain_trajectories().await });
        }
        match answer.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(PersistenceError::Storage(error)),
            Err(_) => Err(PersistenceError::Storage(
                "the trajectory batch ended without an answer".to_string(),
            )),
        }
    }

    async fn drain_trajectories(&self) {
        while let Some(batch) = self.trajectory_batch.next_batch() {
            let rows: Vec<&TrajectoryRow> = batch.iter().map(|waiting| &waiting.row).collect();
            let result = self.write_trajectory_batch(&rows).await;
            if let Err(error) = &result {
                tracing::error!(rows = rows.len(), %error, "trajectory batch not written");
            }
            for waiting in batch {
                let _ = waiting
                    .written
                    .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            }
        }
    }

    async fn write_trajectory_batch(
        &self,
        rows: &[&TrajectoryRow],
    ) -> Result<(), PersistenceError> {
        retry_persistence_with_max_attempts("turso.persist_trajectory", BATCH_ATTEMPTS, || {
            self.write_trajectory_rows(rows)
        })
        .await
    }

    async fn write_trajectory_rows(&self, rows: &[&TrajectoryRow]) -> Result<(), PersistenceError> {
        let conn = self
            .write_connection("turso.persist_trajectory", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            let mut sql = String::from(
                "INSERT INTO trajectories \
                 (tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                  agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                  created_at, request_body, intent, matched_policy_ids, capture_seq) VALUES ",
            );
            let mut values = Vec::with_capacity(chunk.len() * 20);
            for (index, row) in chunk.iter().enumerate() {
                if index > 0 {
                    sql.push_str(", ");
                }
                sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)");
                values.extend(row.0.iter().cloned());
            }
            tx.execute(&sql, params_from_iter(values))
                .await
                .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }
}
