//! OTS trajectory persistence methods.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use crate::metrics::TursoQueryTimer;

/// Row returned by OTS trajectory list queries (metadata only, not full data).
#[derive(Debug, Clone, serde::Serialize)]
pub struct OtsTrajectoryRow {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub persistence_status: String,
    pub persist_attempts: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A stored OTS trajectory document together with the run identity recorded
/// alongside it.
///
/// The document itself carries no session or tenant — those live on the row —
/// so any consumer that needs the run identity would otherwise have to list
/// the table to find what it already asked for by id.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OtsTrajectoryDocument {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub data: String,
}

/// Durable queued OTS trajectory row ready for outbox replay.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OtsQueuedTrajectoryRow {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub data: String,
    pub persist_attempts: i64,
}

/// Parameters for persisting an OTS trajectory.
pub struct OtsTrajectoryParams<'a> {
    pub trajectory_id: &'a str,
    pub tenant: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub outcome: &'a str,
    pub turn_count: i64,
    pub data: &'a str,
}

impl TursoEventStore {
    /// Persist a full OTS trajectory JSON blob.
    ///
    /// Identity is `(tenant, trajectory_id)`: the id comes from the uploading
    /// harness and one store holds every tenant's rows, so keying on the id
    /// alone would let one tenant's upload replace another's row — tenant
    /// column included.
    #[instrument(skip_all, fields(
        otel.name = "turso.persist_ots_trajectory",
        trajectory_id = %p.trajectory_id,
        agent_id = %p.agent_id,
    ))]
    pub async fn persist_ots_trajectory(
        &self,
        p: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.persist_ots_trajectory");
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, persistence_status, persist_attempts, last_error, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'persisted', 0, NULL, datetime('now'), datetime('now')) \
             ON CONFLICT(tenant, trajectory_id) DO UPDATE SET \
                agent_id = excluded.agent_id, session_id = excluded.session_id, \
                outcome = excluded.outcome, turn_count = excluded.turn_count, data = excluded.data, \
                persistence_status = 'persisted', persist_attempts = 0, last_error = NULL, \
                updated_at = datetime('now')",
            params![
                p.trajectory_id.to_string(),
                p.tenant.to_string(),
                p.agent_id.to_string(),
                p.session_id.to_string(),
                p.outcome.to_string(),
                p.turn_count,
                p.data.to_string(),
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Durably admit an OTS trajectory artifact for background status advancement.
    #[instrument(skip_all, fields(
        otel.name = "turso.enqueue_ots_trajectory",
        trajectory_id = %p.trajectory_id,
        agent_id = %p.agent_id,
    ))]
    pub async fn enqueue_ots_trajectory(
        &self,
        p: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.enqueue_ots_trajectory");
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, persistence_status, persist_attempts, last_error, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 0, NULL, datetime('now'), datetime('now')) \
             ON CONFLICT(tenant, trajectory_id) DO UPDATE SET \
                agent_id = excluded.agent_id, session_id = excluded.session_id, \
                outcome = excluded.outcome, turn_count = excluded.turn_count, data = excluded.data, \
                persistence_status = 'queued', last_error = NULL, updated_at = datetime('now')",
            params![
                p.trajectory_id.to_string(),
                p.tenant.to_string(),
                p.agent_id.to_string(),
                p.session_id.to_string(),
                p.outcome.to_string(),
                p.turn_count,
                p.data.to_string(),
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Mark a queued OTS trajectory as persisted.
    ///
    /// Addressed by the same `(tenant, trajectory_id)` identity the row is
    /// keyed by: two tenants may hold the same id, and an unscoped update would
    /// declare both of them persisted.
    pub async fn mark_ots_trajectory_persisted(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.mark_ots_trajectory_persisted");
        let conn = self.connection()?;
        conn.execute(
            "UPDATE ots_trajectories \
             SET persistence_status = 'persisted', last_error = NULL, updated_at = datetime('now') \
             WHERE tenant = ?1 AND trajectory_id = ?2",
            params![tenant.to_string(), trajectory_id.to_string()],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Mark a queued OTS trajectory as failed after retries exhaust.
    pub async fn mark_ots_trajectory_failed(
        &self,
        tenant: &str,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.mark_ots_trajectory_failed");
        let conn = self.connection()?;
        conn.execute(
            "UPDATE ots_trajectories \
             SET persistence_status = 'failed', persist_attempts = persist_attempts + 1, last_error = ?3, updated_at = datetime('now') \
             WHERE tenant = ?1 AND trajectory_id = ?2",
            params![
                tenant.to_string(),
                trajectory_id.to_string(),
                error.to_string()
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List queued OTS trajectory artifacts for startup replay.
    pub async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.list_queued_ots_trajectories");
        let conn = self.connection()?;
        let mut rows = conn
            .query(
                "SELECT trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, persist_attempts \
                 FROM ots_trajectories \
                 WHERE persistence_status = 'queued' \
                 ORDER BY updated_at ASC, created_at ASC \
                 LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(storage_error)?;

        let mut result = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            result.push(OtsQueuedTrajectoryRow {
                trajectory_id: row.get(0).unwrap_or_default(),
                tenant: row.get(1).unwrap_or_default(),
                agent_id: row.get(2).unwrap_or_default(),
                session_id: row.get(3).unwrap_or_default(),
                outcome: row.get(4).unwrap_or_default(),
                turn_count: row.get(5).unwrap_or(0),
                data: row.get(6).unwrap_or_default(),
                persist_attempts: row.get(7).unwrap_or(0),
            });
        }
        Ok(result)
    }

    /// List OTS trajectories (metadata only, without full data blob).
    #[instrument(skip_all, fields(otel.name = "turso.list_ots_trajectories"))]
    pub async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.list_ots_trajectories");
        let conn = self.connection()?;

        // Build query with optional filters.
        let mut sql = String::from(
            "SELECT trajectory_id, tenant, agent_id, session_id, outcome, turn_count, persistence_status, persist_attempts, last_error, created_at, updated_at FROM ots_trajectories WHERE tenant = ?1",
        );
        let mut idx = 2;
        if agent_id.is_some() {
            sql.push_str(&format!(" AND agent_id = ?{idx}"));
            idx += 1;
        }
        if outcome.is_some() {
            sql.push_str(&format!(" AND outcome = ?{idx}"));
        }
        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT {limit}"));

        let mut values: Vec<libsql::Value> = vec![tenant.to_string().into()];
        if let Some(aid) = agent_id {
            values.push(aid.to_string().into());
        }
        if let Some(out) = outcome {
            values.push(out.to_string().into());
        }

        let mut rows = conn
            .query(&sql, libsql::params_from_iter(values))
            .await
            .map_err(storage_error)?;

        let mut result = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            result.push(OtsTrajectoryRow {
                trajectory_id: row.get(0).unwrap_or_default(),
                tenant: row.get(1).unwrap_or_default(),
                agent_id: row.get(2).unwrap_or_default(),
                session_id: row.get(3).unwrap_or_default(),
                outcome: row.get(4).unwrap_or_default(),
                turn_count: row.get(5).unwrap_or(0),
                persistence_status: row.get(6).unwrap_or_else(|_| "persisted".to_string()),
                persist_attempts: row.get(7).unwrap_or(0),
                last_error: row.get(8).ok(),
                created_at: row.get(9).unwrap_or_default(),
                updated_at: row.get(10).unwrap_or_default(),
            });
        }

        Ok(result)
    }

    /// Load full OTS trajectory data by tenant and ID.
    ///
    /// The tenant is part of the lookup rather than a post-filter: one store
    /// can hold every tenant's rows, so a caller that takes the trajectory id
    /// from a request path would otherwise read across tenants.
    #[instrument(skip_all, fields(otel.name = "turso.get_ots_trajectory"))]
    pub async fn get_ots_trajectory(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<OtsTrajectoryDocument>, PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.get_ots_trajectory");
        let conn = self.connection()?;
        let mut rows = conn
            .query(
                "SELECT agent_id, COALESCE(session_id, ''), outcome, data \
                 FROM ots_trajectories WHERE tenant = ?1 AND trajectory_id = ?2",
                params![tenant.to_string(), trajectory_id.to_string()],
            )
            .await
            .map_err(storage_error)?;

        if let Some(row) = rows.next().await.map_err(storage_error)? {
            Ok(Some(OtsTrajectoryDocument {
                trajectory_id: trajectory_id.to_string(),
                tenant: tenant.to_string(),
                agent_id: row.get(0).unwrap_or_default(),
                session_id: row.get(1).unwrap_or_default(),
                outcome: row.get(2).unwrap_or_default(),
                data: row.get(3).unwrap_or_default(),
            }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "ots_test.rs"]
mod tests;
