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
mod tests {
    use super::*;

    async fn test_store() -> (TursoEventStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("ots-outbox.db");
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso store");
        (store, dir)
    }

    fn params<'a>(trajectory_id: &'a str, data: &'a str) -> OtsTrajectoryParams<'a> {
        tenant_params("tenant", trajectory_id, data)
    }

    fn tenant_params<'a>(
        tenant: &'a str,
        trajectory_id: &'a str,
        data: &'a str,
    ) -> OtsTrajectoryParams<'a> {
        OtsTrajectoryParams {
            trajectory_id,
            tenant,
            agent_id: "agent",
            session_id: "session",
            outcome: "success",
            turn_count: 2,
            data,
        }
    }

    #[tokio::test]
    async fn ots_outbox_status_lifecycle_is_durable() {
        let (store, _dir) = test_store().await;
        let data = r#"{"trajectory_id":"traj-durable","turns":[]}"#;

        store
            .enqueue_ots_trajectory(&params("traj-durable", data))
            .await
            .expect("enqueue trajectory");

        let rows = store
            .list_ots_trajectories("tenant", None, None, 10)
            .await
            .expect("list trajectories");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].persistence_status, "queued");

        let queued = store
            .list_queued_ots_trajectories(10)
            .await
            .expect("list queued trajectories");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].trajectory_id, "traj-durable");
        assert_eq!(queued[0].data, data);

        store
            .mark_ots_trajectory_persisted("tenant", "traj-durable")
            .await
            .expect("mark persisted");
        let rows = store
            .list_ots_trajectories("tenant", None, None, 10)
            .await
            .expect("list persisted trajectory");
        assert_eq!(rows[0].persistence_status, "persisted");
        assert!(rows[0].last_error.is_none());

        store
            .enqueue_ots_trajectory(&params("traj-durable", data))
            .await
            .expect("requeue trajectory");
        store
            .mark_ots_trajectory_failed("tenant", "traj-durable", "transient")
            .await
            .expect("mark failed");
        let rows = store
            .list_ots_trajectories("tenant", None, None, 10)
            .await
            .expect("list failed trajectory");
        assert_eq!(rows[0].persistence_status, "failed");
        assert_eq!(rows[0].persist_attempts, 1);
        assert_eq!(rows[0].last_error.as_deref(), Some("transient"));
    }

    #[tokio::test]
    async fn get_ots_trajectory_is_scoped_to_its_tenant() {
        let (store, _dir) = test_store().await;
        let data = r#"{"trajectory_id":"traj-tenant-a","turns":[]}"#;
        store
            .persist_ots_trajectory(&params("traj-tenant-a", data))
            .await
            .expect("persist trajectory");

        let document = store
            .get_ots_trajectory("tenant", "traj-tenant-a")
            .await
            .expect("read own tenant")
            .expect("document present");
        assert_eq!(document.data, data);
        assert_eq!(document.session_id, "session");
        assert_eq!(document.agent_id, "agent");
        assert!(
            store
                .get_ots_trajectory("other-tenant", "traj-tenant-a")
                .await
                .expect("read foreign tenant")
                .is_none(),
            "a foreign tenant must not read another tenant's trajectory by id"
        );
    }

    #[tokio::test]
    async fn one_tenant_s_upload_cannot_replace_another_s_row_with_the_same_id() {
        // The trajectory id is chosen by the uploading harness, so two tenants
        // colliding on one is an ordinary event, not an attack.
        let (store, _dir) = test_store().await;
        let alpha = r#"{"trajectory_id":"traj-shared","turns":[],"owner":"alpha"}"#;
        let beta = r#"{"trajectory_id":"traj-shared","turns":[],"owner":"beta"}"#;

        store
            .persist_ots_trajectory(&tenant_params("alpha", "traj-shared", alpha))
            .await
            .expect("persist alpha");
        store
            .persist_ots_trajectory(&tenant_params("beta", "traj-shared", beta))
            .await
            .expect("persist beta");

        let alpha_row = store
            .get_ots_trajectory("alpha", "traj-shared")
            .await
            .expect("read alpha")
            .expect("alpha still has its row");
        assert_eq!(
            alpha_row.data, alpha,
            "a second tenant's upload must not overwrite the first tenant's trajectory"
        );
        let beta_row = store
            .get_ots_trajectory("beta", "traj-shared")
            .await
            .expect("read beta")
            .expect("beta has its own row");
        assert_eq!(beta_row.data, beta);
    }

    #[tokio::test]
    async fn marking_one_tenant_s_trajectory_leaves_another_s_alone() {
        let (store, _dir) = test_store().await;
        let data = r#"{"trajectory_id":"traj-shared","turns":[]}"#;
        store
            .enqueue_ots_trajectory(&tenant_params("alpha", "traj-shared", data))
            .await
            .expect("enqueue alpha");
        store
            .enqueue_ots_trajectory(&tenant_params("beta", "traj-shared", data))
            .await
            .expect("enqueue beta");

        store
            .mark_ots_trajectory_failed("alpha", "traj-shared", "transient")
            .await
            .expect("mark alpha failed");

        let beta = store
            .list_ots_trajectories("beta", None, None, 10)
            .await
            .expect("list beta");
        assert_eq!(beta.len(), 1);
        assert_eq!(
            beta[0].persistence_status, "queued",
            "a status update addressed at one tenant must not land on another's row"
        );
        assert!(beta[0].last_error.is_none());
    }

    #[tokio::test]
    async fn a_globally_keyed_table_is_rekeyed_by_tenant_on_open() {
        // Databases created before the identity fix carry a table keyed on
        // `trajectory_id` alone. Opening the store has to rekey it, keeping
        // the rows it already holds.
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("legacy-ots.db");
        let db_url = format!("file:{}", db_path.display());

        {
            let legacy = libsql::Builder::new_local(&db_path)
                .build()
                .await
                .expect("open legacy db");
            let conn = legacy.connect().expect("connect legacy db");
            conn.execute(
                "CREATE TABLE ots_trajectories (
                    trajectory_id TEXT PRIMARY KEY,
                    tenant TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    session_id TEXT,
                    outcome TEXT NOT NULL DEFAULT 'unknown',
                    entity_type TEXT,
                    turn_count INTEGER NOT NULL DEFAULT 0,
                    data TEXT NOT NULL,
                    persistence_status TEXT NOT NULL DEFAULT 'persisted',
                    persist_attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
                (),
            )
            .await
            .expect("create legacy table");
            conn.execute(
                "INSERT INTO ots_trajectories \
                 (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data) \
                 VALUES ('traj-legacy', 'alpha', 'agent', 'session', 'success', 1, '{}')",
                (),
            )
            .await
            .expect("seed legacy row");
        }

        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("open the store, running the rekey");

        assert!(
            store
                .get_ots_trajectory("alpha", "traj-legacy")
                .await
                .expect("read the carried-over row")
                .is_some(),
            "the rekey must carry existing rows across"
        );
        store
            .persist_ots_trajectory(&tenant_params(
                "beta",
                "traj-legacy",
                "{\"owner\":\"beta\"}",
            ))
            .await
            .expect("a second tenant can now hold the same id");
        assert_eq!(
            store
                .get_ots_trajectory("alpha", "traj-legacy")
                .await
                .expect("read alpha")
                .expect("alpha kept its row")
                .data,
            "{}",
            "the rekeyed table must not let one tenant clobber another"
        );
    }
}
