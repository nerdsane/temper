//! Trajectory persistence and query methods.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{ActionStats, AgentSummary, TrajectoryStats, TursoEventStore, TursoTrajectoryRow};
use crate::TursoTrajectoryInsert;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Persist a trajectory entry (all columns including agent/authz fields).
    #[instrument(skip_all, fields(otel.name = "turso.persist_trajectory"))]
    pub async fn persist_trajectory(
        &self,
        entry: TursoTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.persist_trajectory");
        let conn = self.configured_connection().await?;
        let execute_res = conn
            .execute(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
              agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                entry.created_at
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

    /// Load recent trajectory entries (newest first, up to `limit`).
    #[instrument(skip_all, fields(otel.name = "turso.load_recent_trajectories"))]
    pub async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_recent_trajectories");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                        agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, created_at \
                 FROM trajectories \
                 ORDER BY created_at DESC \
                 LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(|e| {
                let error = storage_error(e);
                tracing::warn!(limit, error = %error, "trajectory.store.read");
                error
            })?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_trajectory(&row)?);
        }
        tracing::debug!(limit, count = out.len(), "trajectory.store.read");
        Ok(out)
    }

    /// Parse a trajectory row from a libsql Row (16 columns).
    fn row_to_trajectory(row: &libsql::Row) -> Result<TursoTrajectoryRow, PersistenceError> {
        Ok(TursoTrajectoryRow {
            tenant: row.get::<String>(0).map_err(storage_error)?,
            entity_type: row.get::<String>(1).map_err(storage_error)?,
            entity_id: row.get::<String>(2).map_err(storage_error)?,
            action: row.get::<String>(3).map_err(storage_error)?,
            success: row.get::<i64>(4).map_err(storage_error)? != 0,
            from_status: row.get::<Option<String>>(5).map_err(storage_error)?,
            to_status: row.get::<Option<String>>(6).map_err(storage_error)?,
            error: row.get::<Option<String>>(7).map_err(storage_error)?,
            agent_id: row.get::<Option<String>>(8).map_err(storage_error)?,
            session_id: row.get::<Option<String>>(9).map_err(storage_error)?,
            authz_denied: row
                .get::<Option<i64>>(10)
                .map_err(storage_error)?
                .map(|v| v != 0),
            denied_resource: row.get::<Option<String>>(11).map_err(storage_error)?,
            denied_module: row.get::<Option<String>>(12).map_err(storage_error)?,
            source: row.get::<Option<String>>(13).map_err(storage_error)?,
            spec_governed: row
                .get::<Option<i64>>(14)
                .map_err(storage_error)?
                .map(|v| v != 0),
            created_at: row.get::<String>(15).map_err(storage_error)?,
        })
    }

    /// Query trajectory statistics with optional filters.
    #[instrument(skip_all, fields(otel.name = "turso.query_trajectory_stats"))]
    pub async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.query_trajectory_stats");
        let conn = self.configured_connection().await?;

        // Total + success count.
        let mut rows = conn
            .query(
                "SELECT COUNT(*) AS total, \
                        COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END), 0) AS success_count \
                 FROM trajectories \
                 WHERE (?1 IS NULL OR entity_type = ?1) \
                   AND (?2 IS NULL OR action = ?2) \
                   AND (?3 IS NULL OR success = ?3)",
                params![entity_type, action, success_filter.map(|b| b as i64)],
            )
            .await
            .map_err(storage_error)?;

        let (total, success_count) = match rows.next().await.map_err(storage_error)? {
            Some(row) => (
                row.get::<i64>(0).map_err(storage_error)? as u64,
                row.get::<i64>(1).map_err(storage_error)? as u64,
            ),
            None => (0, 0),
        };
        drop(rows);

        // Per-action breakdown.
        let mut rows = conn
            .query(
                "SELECT action, COUNT(*) AS total, \
                        COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END), 0) AS success, \
                        COALESCE(SUM(CASE WHEN success = 0 THEN 1 ELSE 0 END), 0) AS error \
                 FROM trajectories \
                 GROUP BY action",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut by_action = std::collections::BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let name = row.get::<String>(0).map_err(storage_error)?;
            by_action.insert(
                name,
                ActionStats {
                    total: row.get::<i64>(1).map_err(storage_error)? as u64,
                    success: row.get::<i64>(2).map_err(storage_error)? as u64,
                    error: row.get::<i64>(3).map_err(storage_error)? as u64,
                },
            );
        }
        drop(rows);

        // Failed intents (newest first).
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                        agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, created_at \
                 FROM trajectories \
                 WHERE success = 0 \
                 ORDER BY created_at DESC \
                 LIMIT ?1",
                params![failed_limit],
            )
            .await
            .map_err(storage_error)?;

        let mut failed_intents = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            failed_intents.push(Self::row_to_trajectory(&row)?);
        }

        let error_count = total.saturating_sub(success_count);
        tracing::info!(
            entity_type,
            action,
            success_filter,
            total,
            success_count,
            error_count,
            failed_limit,
            "trajectory.store.read"
        );
        Ok(TrajectoryStats {
            total,
            success_count,
            error_count,
            success_rate: if total > 0 {
                success_count as f64 / total as f64
            } else {
                0.0
            },
            by_action,
            failed_intents,
        })
    }

    /// Query trajectories for a specific agent.
    #[instrument(skip_all, fields(agent_id, otel.name = "turso.query_trajectories_by_agent"))]
    pub async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.query_trajectories_by_agent");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                        agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, created_at \
                 FROM trajectories \
                 WHERE agent_id = ?1 \
                   AND (?2 IS NULL OR tenant = ?2) \
                   AND (?3 IS NULL OR entity_type = ?3) \
                 ORDER BY created_at DESC \
                 LIMIT ?4",
                params![agent_id, tenant, entity_type, limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_trajectory(&row)?);
        }
        tracing::info!(
            agent_id,
            tenant,
            entity_type,
            limit,
            count = out.len(),
            "trajectory.store.read"
        );
        Ok(out)
    }

    /// Query agent summaries (grouped by agent_id).
    #[instrument(skip_all, fields(otel.name = "turso.query_agent_summaries"))]
    pub async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.query_agent_summaries");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT agent_id, \
                        COUNT(*) AS total_actions, \
                        COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END), 0) AS success_count, \
                        COALESCE(SUM(CASE WHEN success = 0 THEN 1 ELSE 0 END), 0) AS error_count, \
                        COALESCE(SUM(CASE WHEN authz_denied = 1 THEN 1 ELSE 0 END), 0) AS denial_count, \
                        MAX(created_at) AS last_active_at \
                 FROM trajectories \
                 WHERE agent_id IS NOT NULL \
                   AND (?1 IS NULL OR tenant = ?1) \
                 GROUP BY agent_id \
                 ORDER BY last_active_at DESC",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let total = row.get::<i64>(1).map_err(storage_error)? as u64;
            let success = row.get::<i64>(2).map_err(storage_error)? as u64;
            out.push(AgentSummary {
                agent_id: row.get::<String>(0).map_err(storage_error)?,
                total_actions: total,
                success_count: success,
                error_count: row.get::<i64>(3).map_err(storage_error)? as u64,
                denial_count: row.get::<i64>(4).map_err(storage_error)? as u64,
                success_rate: if total > 0 {
                    success as f64 / total as f64
                } else {
                    0.0
                },
                last_active_at: row.get::<String>(5).map_err(storage_error)?,
            });
        }
        tracing::info!(tenant, count = out.len(), "trajectory.store.read");
        Ok(out)
    }
}
