//! OTS trajectory storage — full agent execution traces.

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_json, storage_error};
use crate::PostgresEventStore;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresOtsTrajectoryRow {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresOtsTrajectoryParams<'a> {
    pub trajectory_id: &'a str,
    pub tenant: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub outcome: &'a str,
    pub turn_count: i64,
    pub data: &'a str,
}

impl PostgresEventStore {
    pub async fn persist_ots_trajectory(
        &self,
        p: &PostgresOtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(p.data)?;
        crate::dbm::postgres_query!(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
             ON CONFLICT (trajectory_id) DO UPDATE SET \
                 tenant = EXCLUDED.tenant, agent_id = EXCLUDED.agent_id, session_id = EXCLUDED.session_id, \
                 outcome = EXCLUDED.outcome, turn_count = EXCLUDED.turn_count, data = EXCLUDED.data, created_at = now()",
        )
        .bind(p.trajectory_id)
        .bind(p.tenant)
        .bind(p.agent_id)
        .bind(p.session_id)
        .bind(p.outcome)
        .bind(p.turn_count)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresOtsTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT trajectory_id, tenant, agent_id, COALESCE(session_id, '') AS session_id, outcome, turn_count, created_at \
             FROM ots_trajectories \
             WHERE tenant = $1 \
               AND ($2::text IS NULL OR agent_id = $2) \
               AND ($3::text IS NULL OR outcome = $3) \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(tenant)
        .bind(agent_id)
        .bind(outcome)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_ots_trajectory).collect())
    }

    pub async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let row: Option<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM ots_trajectories WHERE trajectory_id = $1"
        )
        .bind(trajectory_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(|value| value.to_string()))
    }
}

fn row_to_ots_trajectory(row: sqlx::postgres::PgRow) -> PostgresOtsTrajectoryRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    PostgresOtsTrajectoryRow {
        trajectory_id: row.get("trajectory_id"),
        tenant: row.get("tenant"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        outcome: row.get("outcome"),
        turn_count: row.get("turn_count"),
        created_at: created_at.to_rfc3339(),
    }
}
