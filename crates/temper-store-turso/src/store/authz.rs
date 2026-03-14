//! Authorization decisions and Cedar policy persistence.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Query decisions for a specific tenant with optional status filter.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.query_decisions"))]
    pub async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.query_decisions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM pending_decisions \
                 WHERE tenant = ?1 AND (?2 IS NULL OR status = ?2) \
                 ORDER BY created_at DESC",
                params![tenant, status],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    /// Query all decisions across tenants with optional status filter.
    #[instrument(skip_all, fields(otel.name = "turso.query_all_decisions"))]
    pub async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.query_all_decisions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM pending_decisions \
                 WHERE (?1 IS NULL OR status = ?1) \
                 ORDER BY created_at DESC",
                params![status],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    /// Get a single pending decision by ID, returning the full JSON data.
    #[instrument(skip_all, fields(id, otel.name = "turso.get_pending_decision"))]
    pub async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.get_pending_decision");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM pending_decisions WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(storage_error)?;

        match rows.next().await.map_err(storage_error)? {
            Some(row) => Ok(Some(row.get::<String>(0).map_err(storage_error)?)),
            None => Ok(None),
        }
    }

    /// Upsert a pending decision (insert or update).
    #[instrument(skip_all, fields(id, tenant, otel.name = "turso.upsert_pending_decision"))]
    pub async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_pending_decision");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO pending_decisions (id, tenant, status, data, updated_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now')) \
             ON CONFLICT(id) DO UPDATE SET status = ?3, data = ?4, updated_at = datetime('now')",
            params![id, tenant, status, data_json],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all pending decisions (newest first, up to limit).
    #[instrument(skip_all, fields(otel.name = "turso.load_pending_decisions"))]
    pub async fn load_pending_decisions(
        &self,
        limit: i64,
    ) -> Result<Vec<String>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_pending_decisions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM pending_decisions ORDER BY created_at DESC LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    /// Upsert Cedar policy text for a tenant.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.upsert_tenant_policy"))]
    pub async fn upsert_tenant_policy(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_tenant_policy");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
             VALUES (?1, ?2, datetime('now')) \
             ON CONFLICT(tenant) DO UPDATE SET policy_text = ?2, updated_at = datetime('now')",
            params![tenant, policy_text],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all tenant Cedar policies.
    #[instrument(skip_all, fields(otel.name = "turso.load_tenant_policies"))]
    pub async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_tenant_policies");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, policy_text FROM tenant_policies ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }
}
