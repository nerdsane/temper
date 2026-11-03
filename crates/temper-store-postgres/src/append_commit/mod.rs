//! One append and every row its commit writes, in one SQL statement.
//!
//! A move writes its events, its declared keys, its vector rows, the scheduled
//! actions it arms and fires, the record's catalog row and field-index rows,
//! its timer row and its projection-gap mark in a single data-modifying CTE.
//! The statement is its own transaction: it commits all of them or none, in one
//! round trip, and its text never changes, so each connection prepares it once.
//!
//! Every write reads `w`, which has a row only when `chk.ok` holds: the journal
//! head is the expected sequence and no other record holds a key this append
//! claims. A stale expected sequence therefore writes nothing and comes back as
//! `ok = false` with the head it found. Two writers racing past that check meet
//! on the journal's unique key: the second statement aborts, and with it every
//! row it would have written. Every write also reads `ev`'s output through `w`,
//! so the events are inserted before any other row, and a racing writer of the
//! same record fails on the journal rather than on one of its other rows.
//!
//! Every writer of a record's rows takes the record's catalog row first: this
//! statement in `locked` (which every write depends on, through `chk`), the
//! repair writer (`upsert_query_projection_with_state`) by `FOR UPDATE` before
//! its index and mark. After the catalog, this statement writes the events and
//! the record's other rows, then the catalog row, and only then the field
//! index and the gap mark (`cat`, which they join), the order the repair
//! writer takes them in, so the two cannot deadlock. A batch runs this
//! statement per record in persistence-id order. If the catalog row changed
//! between the statement's snapshot and its lock (`raced`: a writer committed
//! while it waited), the snapshot cannot see that writer's field-index rows,
//! so the statement writes nothing and runs once more, on a snapshot taken
//! while its connection already holds the lock (`append_on`), which no writer
//! can then change under it.
//!
//! A record with no catalog row yet has nothing to lock. While the server
//! serves, only the record's own commit creates that row: boot repair creates
//! the missing ones before serving, and the snapshot backfill that runs while
//! serving rewrites existing rows only
//! (`refresh_query_projection_with_state`). Two commits of one record meet on
//! the journal's unique key.
//!
//! The sub-statements of one statement see one snapshot and must not change the
//! same row twice, so each table's writes touch disjoint rows: the field-index
//! delete takes the names the record no longer indexes and the upsert the names
//! it does, the key delete takes a name's old hash and the insert its new one,
//! and so on for segments, vectors, scheduled actions and the gap mark.
//!
//! The rows are the ones the projection writer
//! (`PostgresEventStore::upsert_query_projection_with_state`) writes, byte for
//! byte: the same status, stored state, hash and scalar index.

use std::time::Instant;

use temper_runtime::persistence::{
    CommitRows, EntityKeyRow, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::PostgresEventStore;
use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_projection_index_fields,
};
use crate::platform::storage_error;

mod params;
pub(crate) mod retry_seam;
mod sql;

use params::AppendParams;
pub(crate) use sql::APPEND_COMMIT_SQL;

const EVENT_APPEND_OPERATION: &str = "event_append";

/// Postgres's SQLSTATE for a unique-constraint violation.
const UNIQUE_VIOLATION: &str = "23505";

/// What the statement answers: whether the checks held, the journal head it
/// found, the sequence it wrote at, and the key another record holds.
type AppendAnswer = (bool, i64, Option<i64>, Option<String>, Option<String>, bool);

impl PostgresEventStore {
    /// Append `events` to the record's journal with every row in `rows`, in
    /// one statement. Returns the record's sequence after the append.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::ConcurrencyViolation`] when the journal is not at
    /// `expected_sequence`, or another writer appended first; a storage error
    /// when another record holds a declared key, or the statement fails. Either
    /// way nothing was written.
    pub(crate) async fn append_commit(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        let mut timer = PostgresTransactionTimer::start(EVENT_APPEND_OPERATION);
        let acquire_started = Instant::now();
        let acquired = self.pool().acquire().await;
        record_postgres_pool_acquire_duration(
            acquire_started.elapsed(),
            EVENT_APPEND_OPERATION,
            if acquired.is_ok() { "ok" } else { "error" },
        );
        let mut conn = acquired.map_err(storage_error)?;
        let written = self
            .append_on(
                AppendOn::Alone(&mut conn),
                persistence_id,
                expected_sequence,
                events,
                rows,
                &mut timer,
            )
            .await?;
        timer.set_outcome("ok");
        Ok(written)
    }

    /// Run the append statement for one record on `on`'s connection: on its
    /// own it is one transaction; inside a batch's transaction it is one of
    /// its statements, and the batch commits them together.
    pub(crate) async fn append_on(
        &self,
        on: AppendOn<'_>,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
        timer: &mut PostgresTransactionTimer,
    ) -> Result<u64, PersistenceError> {
        let (conn, in_batch) = match on {
            AppendOn::Alone(conn) => (conn, false),
            AppendOn::InBatch(conn) => (conn, true),
        };
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let params = AppendParams::new(
            [tenant, entity_type, entity_id],
            expected_sequence,
            events,
            rows,
        )?;
        let written = match self
            .run_append(conn, &params, expected_sequence, entity_type, timer)
            .await?
        {
            Some(written) => written,
            // Raced: run once more on a snapshot taken after this connection
            // holds the record's catalog row lock, so no writer can change the
            // row between that snapshot and the statement's own lock, and the
            // second run cannot race. In a batch, the first run's `locked`
            // took that lock and the batch's transaction still holds it. On
            // its own, the first run committed and let it go, so the second
            // takes it first, in a transaction of its own.
            None => {
                let raced_again = || {
                    PersistenceError::Storage(format!(
                        "the {entity_type} record's catalog row changed while this append \
                         held its lock"
                    ))
                };
                if in_batch {
                    self.run_append(conn, &params, expected_sequence, entity_type, timer)
                        .await?
                        .ok_or_else(raced_again)?
                } else {
                    retry_seam::before_retry(self.schema.prefix()).await;
                    let mut tx = sqlx::Connection::begin(&mut *conn)
                        .await
                        .map_err(storage_error)?;
                    if retry_seam::takes_retry_lock(self.schema.prefix()) {
                        let _held: Option<i32> = crate::dbm::postgres_query_scalar!(
                            &self.schema,
                            "SELECT 1 FROM {schema}entity_catalog \
                                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                                 FOR UPDATE",
                        )
                        .bind(tenant)
                        .bind(entity_type)
                        .bind(entity_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                    }
                    // A second race returns before the commit: the
                    // transaction rolls back with nothing written.
                    let written = self
                        .run_append(&mut tx, &params, expected_sequence, entity_type, timer)
                        .await?
                        .ok_or_else(raced_again)?;
                    tx.commit().await.map_err(storage_error)?;
                    written
                }
            }
        };
        if params.projection.op == Some("upsert") {
            record_postgres_projection_index_fields(
                EVENT_APPEND_OPERATION,
                entity_type,
                params.projection.indexed_fields,
                params.projection.skipped_fields,
            );
        }
        debug_assert!(written >= expected_sequence);
        debug_assert_eq!(written, expected_sequence + events.len() as u64);
        Ok(written)
    }

    /// Run the append statement once on `conn`. None when it raced another
    /// writer of the record's rows and wrote nothing.
    async fn run_append(
        &self,
        conn: &mut sqlx::PgConnection,
        params: &AppendParams<'_>,
        expected_sequence: u64,
        entity_type: &str,
        timer: &mut PostgresTransactionTimer,
    ) -> Result<Option<u64>, PersistenceError> {
        retry_seam::ran(self.schema.prefix());
        let answer = params
            .bind(crate::dbm::postgres_query_as!(
                &self.schema,
                APPEND_COMMIT_SQL
            ))
            .fetch_one(&mut *conn)
            .await;
        match answer {
            Ok(answer) => settle(answer, expected_sequence, entity_type, timer),
            Err(error) => {
                let error = append_error(error, expected_sequence, entity_type);
                if matches!(error, PersistenceError::ConcurrencyViolation { .. }) {
                    timer.set_outcome("concurrency_violation");
                }
                Err(error)
            }
        }
    }
}

/// The connection an append runs on, and whether the statement commits on
/// its own or as one statement of a batch's transaction, which decides how a
/// raced run takes its lock again.
pub(crate) enum AppendOn<'c> {
    /// The statement is its own transaction.
    Alone(&'c mut sqlx::PgConnection),
    /// The statement runs inside a batch's transaction.
    InBatch(&'c mut sqlx::PgConnection),
}

/// The sequence the statement wrote at, None when it raced another writer
/// of the record's rows and wrote nothing, or why it refused.
fn settle(
    (ok, head, written, clash_key, clash_holder, raced): AppendAnswer,
    expected_sequence: u64,
    entity_type: &str,
    timer: &mut PostgresTransactionTimer,
) -> Result<Option<u64>, PersistenceError> {
    let head = u64::try_from(head).unwrap_or_default();
    if head != expected_sequence {
        debug_assert!(!ok, "a stale expected sequence never writes");
        timer.set_outcome("concurrency_violation");
        return Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: head,
        });
    }
    if let (Some(key_name), Some(holder)) = (clash_key, clash_holder) {
        debug_assert!(!ok, "a key another record holds is never claimed");
        return Err(PersistenceError::Storage(format!(
            "duplicate declared key '{key_name}' for {entity_type}: held by {holder}"
        )));
    }
    if raced {
        debug_assert!(!ok, "a raced append writes nothing");
        return Ok(None);
    }
    assert!(ok, "an append whose checks held writes its rows");
    let written = written.ok_or_else(|| {
        PersistenceError::Storage("append wrote no sequence although its checks held".into())
    })?;
    Ok(Some(u64::try_from(written).unwrap_or_default()))
}

/// What a failed statement means. Only a unique violation has a meaning of its
/// own: on the journal, another writer appended first; on the key index,
/// another record claimed the key since the statement's snapshot. The table
/// names it, not the message text.
fn append_error(error: sqlx::Error, expected_sequence: u64, entity_type: &str) -> PersistenceError {
    let unique_table = error
        .as_database_error()
        .filter(|db| db.code().as_deref() == Some(UNIQUE_VIOLATION))
        .and_then(|db| db.table().map(str::to_string));
    match unique_table.as_deref() {
        Some("events") => PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: expected_sequence + 1,
        },
        Some("entity_key_index") => PersistenceError::Storage(format!(
            "duplicate declared key for {entity_type}: claimed by another record ({error})"
        )),
        _ => storage_error(error),
    }
}

/// A row of another record would be written under this one.
fn check_same_record(record: [&str; 3], row: [&str; 3]) -> Result<(), PersistenceError> {
    if record == row {
        return Ok(());
    }
    Err(PersistenceError::Storage(format!(
        "an append to {} carries a row of {}",
        record.join(":"),
        row.join(":")
    )))
}

/// Each declared key names one row: one name twice would change it twice.
fn check_distinct_key_names(keys: &[EntityKeyRow]) -> Result<(), PersistenceError> {
    let mut names: Vec<&str> = keys.iter().map(|key| key.key_name.as_str()).collect();
    names.sort_unstable();
    match names.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(PersistenceError::Storage(format!(
            "an append declares key '{}' twice",
            pair[0]
        ))),
        None => Ok(()),
    }
}

/// Sequences are stored as BIGINT; one past its range is past every row.
fn stored_sequence(sequence_nr: u64) -> i64 {
    i64::try_from(sequence_nr).unwrap_or(i64::MAX)
}
