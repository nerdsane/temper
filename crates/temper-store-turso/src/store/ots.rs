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
    pub created_at: String,
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
            "INSERT OR REPLACE INTO ots_trajectories (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
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
            "SELECT trajectory_id, tenant, agent_id, session_id, outcome, turn_count, created_at FROM ots_trajectories WHERE tenant = ?1",
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
                created_at: row.get(6).unwrap_or_default(),
            });
        }

        Ok(result)
    }

    /// Load full OTS trajectory data by ID.
    #[instrument(skip_all, fields(otel.name = "turso.get_ots_trajectory"))]
    pub async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let _timer = TursoQueryTimer::start("turso.get_ots_trajectory");
        let conn = self.connection()?;
        let mut rows = conn
            .query(
                "SELECT data FROM ots_trajectories WHERE trajectory_id = ?1",
                params![trajectory_id.to_string()],
            )
            .await
            .map_err(storage_error)?;

        if let Some(row) = rows.next().await.map_err(storage_error)? {
            let data: String = row.get(0).unwrap_or_default();
            Ok(Some(data))
        } else {
            Ok(None)
        }
    }
}
