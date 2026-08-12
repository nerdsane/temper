//! Trajectory persistence.
//!
//! The read path lives in the sibling `trajectory_queries` module.

use libsql::params;
use std::time::Duration;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::write_gate::WritePriority;
use crate::TursoTrajectoryInsert;
use crate::metrics::TursoQueryTimer;
use crate::retry::retry_persistence_with_max_attempts;

impl TursoEventStore {
    /// Persist a trajectory entry (all columns including agent/authz fields).
    #[instrument(skip_all, fields(
        otel.name = "turso.persist_trajectory",
        entity_type = entry.entity_type,
        action = entry.action,
        success = entry.success,
        rows_written = tracing::field::Empty,
    ))]
    pub async fn persist_trajectory(
        &self,
        entry: TursoTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.persist_trajectory");
        // Retry transient Hrana BLOCKED / stream errors with backoff (ADR-0056).
        // The INSERT itself is not naturally idempotent (no UNIQUE constraint on
        // trajectory rows), so a successful retry after a lost-ACK can produce
        // a duplicate row. Trajectories are append-only forensic records; a
        // duplicate row with identical fields is acceptable and rare. The
        // alternative (losing the trajectory during Turso wobbles) is worse —
        // trajectories are the observability record of what the entity did.
        let attempt_timeout = trajectory_attempt_timeout();
        retry_persistence_with_max_attempts(
            "turso.persist_trajectory",
            trajectory_max_attempts(),
            || async {
                let _write_permit = self
                    .acquire_write_permit("turso.persist_trajectory", WritePriority::Low)
                    .await?;
                tokio::time::timeout(attempt_timeout, async {
                    let conn = self.configured_connection().await?;
                    let execute_res = conn
                        .execute(
                        "INSERT INTO trajectories \
                         (tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                          agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, created_at, request_body, intent, matched_policy_ids, capture_seq) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                        params![
                            entry.tenant,
                            entry.entity_type,
                            entry.entity_id,
                            entry.action,
                            entry.success as i64,
                            entry.from_status,
                            entry.to_status,
                            entry.error,
                            entry.agent_id,
                            entry.session_id,
                            entry.authz_denied.map(|b| b as i64),
                            entry.denied_resource,
                            entry.denied_module,
                            entry.source,
                            entry.spec_governed.map(|b| b as i64),
                            entry.created_at,
                            entry.request_body,
                            entry.intent,
                            entry.matched_policy_ids,
                            entry.capture_seq
                        ],
                    )
                    .await
                    .map_err(storage_error);
                    if let Err(ref error) = execute_res {
                        tracing::warn!(
                            tenant = entry.tenant,
                            entity_type = entry.entity_type,
                            entity_id = entry.entity_id,
                            action = entry.action,
                            success = entry.success,
                            source = ?entry.source,
                            authz_denied = ?entry.authz_denied,
                            error = %error,
                            "trajectory.store.write"
                        );
                    }
                    execute_res?;
                    Ok(())
                })
                .await
                .unwrap_or_else(|_| {
                    tracing::warn!(
                        tenant = entry.tenant,
                        entity_type = entry.entity_type,
                        entity_id = entry.entity_id,
                        action = entry.action,
                        success = entry.success,
                        source = ?entry.source,
                        authz_denied = ?entry.authz_denied,
                        timeout_ms = attempt_timeout.as_millis() as u64,
                        "trajectory.store.write timed out"
                    );
                    Err(PersistenceError::Storage(format!(
                        "turso.persist_trajectory timed out after {}ms",
                        attempt_timeout.as_millis()
                    )))
                })
            },
        )
        .await?;
        tracing::Span::current().record("rows_written", 1u64);
        tracing::info!(
            tenant = entry.tenant,
            entity_type = entry.entity_type,
            entity_id = entry.entity_id,
            action = entry.action,
            success = entry.success,
            source = ?entry.source,
            authz_denied = ?entry.authz_denied,
            "trajectory.store.write"
        );
        Ok(())
    }
}

fn trajectory_attempt_timeout() -> Duration {
    const DEFAULT_TRAJECTORY_ATTEMPT_TIMEOUT_MS: u64 = 1_000;
    const MIN_TRAJECTORY_ATTEMPT_TIMEOUT_MS: u64 = 100;

    let configured = std::env::var("TEMPER_TURSO_TRAJECTORY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TRAJECTORY_ATTEMPT_TIMEOUT_MS);

    Duration::from_millis(configured.max(MIN_TRAJECTORY_ATTEMPT_TIMEOUT_MS))
}

fn trajectory_max_attempts() -> usize {
    const DEFAULT_TRAJECTORY_MAX_ATTEMPTS: usize = 1;

    std::env::var("TEMPER_TURSO_TRAJECTORY_MAX_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TRAJECTORY_MAX_ATTEMPTS)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TursoTrajectoryInsert;

    async fn test_store() -> (TursoEventStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("trajectory-session.db");
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso store");
        (store, dir)
    }

    fn insert<'a>(
        action: &'a str,
        session_id: &'a str,
        created_at: &'a str,
    ) -> TursoTrajectoryInsert<'a> {
        TursoTrajectoryInsert {
            tenant: "tenant",
            entity_type: "Order",
            entity_id: "order-1",
            action,
            success: true,
            from_status: None,
            to_status: None,
            error: None,
            agent_id: Some("agent-1"),
            session_id: Some(session_id),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some("Entity"),
            spec_governed: Some(true),
            created_at,
            request_body: None,
            intent: None,
            matched_policy_ids: None,
            capture_seq: None,
        }
    }

    #[tokio::test]
    async fn session_query_returns_write_order_and_only_that_session() {
        let (store, _dir) = test_store().await;
        // Same `created_at` on the first two rows: the tiebreaker, not the
        // timestamp, has to keep them in write order.
        for entry in [
            insert("AddItem", "session-a", "2026-01-01T00:00:00Z"),
            insert("SubmitOrder", "session-a", "2026-01-01T00:00:00Z"),
            insert("ConfirmOrder", "session-a", "2026-01-01T00:00:01Z"),
            insert("CancelOrder", "session-b", "2026-01-01T00:00:02Z"),
        ] {
            store
                .persist_trajectory(entry)
                .await
                .expect("persist trajectory");
        }

        let rows = store
            .query_trajectories_by_session("session-a", Some("tenant"), None, 100)
            .await
            .expect("query session");
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["AddItem", "SubmitOrder", "ConfirmOrder"]);

        let other_tenant = store
            .query_trajectories_by_session("session-a", Some("elsewhere"), None, 100)
            .await
            .expect("query session for a foreign tenant");
        assert!(other_tenant.is_empty());

        let filtered = store
            .query_trajectories_by_session("session-a", Some("tenant"), Some("Invoice"), 100)
            .await
            .expect("query session with entity filter");
        assert!(filtered.is_empty());
    }

    #[tokio::test]
    async fn capture_order_outranks_insert_order_inside_one_timestamp() {
        // Rows are written by independently spawned tasks, so the row that was
        // captured first can be inserted second. Inside one `created_at` tick
        // the autoincrement id would then replay the run backwards; the
        // capture sequence is what puts it right.
        let (store, _dir) = test_store().await;
        for (action, capture_seq) in [("SubmitOrder", 2i64), ("AddItem", 1i64)] {
            store
                .persist_trajectory(TursoTrajectoryInsert {
                    capture_seq: Some(capture_seq),
                    ..insert(action, "session-race", "2026-01-01T00:00:00Z")
                })
                .await
                .expect("persist trajectory");
        }

        let rows = store
            .query_trajectories_by_session("session-race", Some("tenant"), None, 100)
            .await
            .expect("query session");
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(
            actions,
            vec!["AddItem", "SubmitOrder"],
            "the read must follow capture order, not the order the writes landed"
        );
        assert_eq!(
            rows.iter()
                .filter_map(|r| r.capture_seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn rows_without_a_capture_sequence_still_order_by_write_order() {
        // Rows written before the column existed carry no sequence; they must
        // still read back deterministically rather than in engine-defined
        // NULL order.
        let (store, _dir) = test_store().await;
        for action in ["AddItem", "SubmitOrder", "ConfirmOrder"] {
            store
                .persist_trajectory(insert(action, "session-legacy", "2026-01-01T00:00:00Z"))
                .await
                .expect("persist trajectory");
        }

        let rows = store
            .query_trajectories_by_session("session-legacy", Some("tenant"), None, 100)
            .await
            .expect("query session");
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["AddItem", "SubmitOrder", "ConfirmOrder"]);
        assert!(rows.iter().all(|r| r.capture_seq.is_none()));
    }
}
