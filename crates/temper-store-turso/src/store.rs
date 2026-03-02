//! Turso/libSQL-backed implementation of the [`EventStore`] trait.

use std::sync::Arc;

use libsql::{Builder, Database, TransactionBehavior, params};
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::{
    TursoSpecVerificationUpdate, TursoTrajectoryInsert, TursoWasmInvocationInsert, schema,
};

#[derive(Clone, Debug)]
pub struct TursoEventStore {
    db: Arc<Database>,
}

impl TursoEventStore {
    /// Connect to a Turso database.
    ///
    /// `url`: `"libsql://your-db.turso.io"` or `"file:local.db"` for local SQLite.
    /// `auth_token`: Turso auth token (`None` for local SQLite).
    pub async fn new(url: &str, auth_token: Option<&str>) -> Result<Self, PersistenceError> {
        let db = if url.starts_with("libsql://") {
            let token = auth_token.ok_or_else(|| {
                PersistenceError::Storage("auth token is required for libsql:// URLs".to_string())
            })?;
            Builder::new_remote(url.to_string(), token.to_string())
                .build()
                .await
                .map_err(storage_error)?
        } else {
            let local_path = url.strip_prefix("file:").unwrap_or(url);
            Builder::new_local(local_path)
                .build()
                .await
                .map_err(storage_error)?
        };

        let store = Self { db: Arc::new(db) };
        store.migrate().await?;
        Ok(store)
    }

    /// Obtain a connection configured for local-SQLite concurrency.
    ///
    /// WAL mode is set in `migrate()` (persists in the DB file). `busy_timeout`
    /// is a per-connection setting — 30 s gives concurrent verification threads
    /// time to wait for the write lock instead of immediately returning SQLITE_BUSY.
    async fn configured_connection(&self) -> Result<libsql::Connection, PersistenceError> {
        let conn = self.db.connect().map_err(storage_error)?;
        // busy_timeout returns the old value as a row — use query() and drop it.
        let _ = conn
            .query("PRAGMA busy_timeout=30000", ())
            .await
            .map_err(storage_error)?;
        Ok(conn)
    }

    /// Run schema migrations on connect.
    async fn migrate(&self) -> Result<(), PersistenceError> {
        let conn = self.connection()?;

        // WAL journal mode lets concurrent readers proceed while a writer holds the
        // lock, and allows multiple writers to serialise without SQLITE_BUSY errors
        // (combined with busy_timeout). The setting persists in the DB file.
        //
        // Both PRAGMAs return a row — use query() and drop the result set.
        let _ = conn
            .query("PRAGMA journal_mode=WAL", ())
            .await
            .map_err(storage_error)?;
        let _ = conn
            .query("PRAGMA busy_timeout=30000", ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVENTS_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SPECS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_SUCCESS_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_CONSTRAINTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_MODULES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX, ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_PENDING_DECISIONS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PENDING_DECISIONS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PENDING_DECISIONS_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_TENANT_POLICIES_TABLE, ())
            .await
            .map_err(storage_error)?;

        // Phase 0: New tables for Turso-as-single-source-of-truth.
        conn.execute(schema::CREATE_FEATURE_REQUESTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TYPE_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_DESIGN_TIME_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;

        // Trajectory table extensions — ALTER TABLE to add missing columns.
        // SQLite returns an error for duplicate columns, so we ignore failures.
        for stmt in &[
            schema::ALTER_TRAJECTORIES_ADD_AGENT_ID,
            schema::ALTER_TRAJECTORIES_ADD_SESSION_ID,
            schema::ALTER_TRAJECTORIES_ADD_AUTHZ_DENIED,
            schema::ALTER_TRAJECTORIES_ADD_DENIED_RESOURCE,
            schema::ALTER_TRAJECTORIES_ADD_DENIED_MODULE,
            schema::ALTER_TRAJECTORIES_ADD_SOURCE,
            schema::ALTER_TRAJECTORIES_ADD_SPEC_GOVERNED,
        ] {
            let _ = conn.execute(*stmt, ()).await; // ignore "duplicate column" errors
        }
        conn.execute(schema::CREATE_TRAJECTORIES_AGENT_INDEX, ())
            .await
            .map_err(storage_error)?;

        Ok(())
    }

    /// Upsert a spec source (IOA + CSDL) for a tenant/entity_type.
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, 0, 'pending', datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 version = specs.version + 1,
                 verified = 0,
                 verification_status = 'pending',
                 levels_passed = NULL,
                 levels_total = NULL,
                 verification_result = NULL,
                 updated_at = datetime('now')",
            params![tenant, entity_type, ioa_source, csdl_xml],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Persist verification result for a spec.
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: TursoSpecVerificationUpdate<'_>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "UPDATE specs SET
                 verified = ?3,
                 verification_status = ?4,
                 levels_passed = ?5,
                 levels_total = ?6,
                 verification_result = ?7,
                 updated_at = datetime('now')
             WHERE tenant = ?1 AND entity_type = ?2",
            params![
                tenant,
                entity_type,
                update.verified as i64,
                update.status,
                update.levels_passed,
                update.levels_total,
                update.verification_result_json
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all persisted specs (for startup recovery).
    pub async fn load_specs(&self) -> Result<Vec<TursoSpecRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                        levels_passed, levels_total, verification_result, updated_at \
                 FROM specs \
                 ORDER BY tenant, entity_type",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoSpecRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                ioa_source: row.get::<String>(2).map_err(storage_error)?,
                csdl_xml: row.get::<Option<String>>(3).map_err(storage_error)?,
                verification_status: row.get::<String>(4).map_err(storage_error)?,
                verified: row.get::<i64>(5).map_err(storage_error)? != 0,
                levels_passed: row
                    .get::<Option<i64>>(6)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                levels_total: row
                    .get::<Option<i64>>(7)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                verification_result: row.get::<Option<String>>(8).map_err(storage_error)?,
                updated_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Persist a trajectory entry (all columns including agent/authz fields).
    pub async fn persist_trajectory(
        &self,
        entry: TursoTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
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
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load recent trajectory entries (newest first, up to `limit`).
    pub async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
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
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_trajectory(&row)?);
        }
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

    // -----------------------------------------------------------------------
    // Trajectory query methods (Phase 1B)
    // -----------------------------------------------------------------------

    /// Query trajectory statistics with optional filters.
    pub async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
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
    pub async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
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
        Ok(out)
    }

    /// Query agent summaries (grouped by agent_id).
    pub async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
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
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Feature request CRUD (Phase 1C)
    // -----------------------------------------------------------------------

    /// Upsert a feature request.
    pub async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO feature_requests (id, category, description, frequency, trajectory_refs, disposition, developer_notes, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now')) \
             ON CONFLICT(id) DO UPDATE SET \
                 category = ?2, description = ?3, frequency = ?4, trajectory_refs = ?5, \
                 disposition = ?6, developer_notes = ?7, updated_at = datetime('now')",
            params![id, category, description, frequency, trajectory_refs_json, disposition, developer_notes],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List feature requests with optional disposition filter.
    pub async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at \
                 FROM feature_requests \
                 WHERE (?1 IS NULL OR disposition = ?1) \
                 ORDER BY frequency DESC, created_at DESC",
                params![disposition],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(FeatureRequestRow {
                id: row.get::<String>(0).map_err(storage_error)?,
                category: row.get::<String>(1).map_err(storage_error)?,
                description: row.get::<String>(2).map_err(storage_error)?,
                frequency: row.get::<i64>(3).map_err(storage_error)?,
                trajectory_refs: row.get::<String>(4).map_err(storage_error)?,
                disposition: row.get::<String>(5).map_err(storage_error)?,
                developer_notes: row.get::<Option<String>>(6).map_err(storage_error)?,
                created_at: row.get::<String>(7).map_err(storage_error)?,
                updated_at: row.get::<String>(8).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Update a feature request's disposition and developer notes.
    pub async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "UPDATE feature_requests SET disposition = ?2, developer_notes = ?3, updated_at = datetime('now') \
                 WHERE id = ?1",
                params![id, disposition, developer_notes],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Evolution record CRUD (Phase 1D)
    // -----------------------------------------------------------------------

    /// Insert an evolution record.
    pub async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO evolution_records (id, record_type, status, created_by, derived_from, data, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![id, record_type, status, created_by, derived_from, data_json],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Get a single evolution record by ID.
    pub async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, record_type, status, created_by, derived_from, data, timestamp \
                 FROM evolution_records WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };
        Ok(Some(Self::row_to_evolution_record(&row)?))
    }

    /// List evolution records with optional type and status filters.
    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, record_type, status, created_by, derived_from, data, timestamp \
                 FROM evolution_records \
                 WHERE (?1 IS NULL OR record_type = ?1) \
                   AND (?2 IS NULL OR status = ?2) \
                 ORDER BY timestamp DESC",
                params![record_type, status],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_evolution_record(&row)?);
        }
        Ok(out)
    }

    /// List ranked insights (Insight type, sorted by priority_score in data).
    pub async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, record_type, status, created_by, derived_from, data, timestamp \
                 FROM evolution_records \
                 WHERE record_type = 'Insight' \
                 ORDER BY timestamp DESC",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_evolution_record(&row)?);
        }
        // Sort by priority_score descending (extracted from JSON data).
        out.sort_by(|a, b| {
            let score_a = serde_json::from_str::<serde_json::Value>(&a.data)
                .ok()
                .and_then(|v| v.get("priority_score").and_then(|s| s.as_f64()))
                .unwrap_or(0.0);
            let score_b = serde_json::from_str::<serde_json::Value>(&b.data)
                .ok()
                .and_then(|v| v.get("priority_score").and_then(|s| s.as_f64()))
                .unwrap_or(0.0);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// Parse an evolution record row.
    fn row_to_evolution_record(row: &libsql::Row) -> Result<EvolutionRecordRow, PersistenceError> {
        Ok(EvolutionRecordRow {
            id: row.get::<String>(0).map_err(storage_error)?,
            record_type: row.get::<String>(1).map_err(storage_error)?,
            status: row.get::<String>(2).map_err(storage_error)?,
            created_by: row.get::<String>(3).map_err(storage_error)?,
            derived_from: row.get::<Option<String>>(4).map_err(storage_error)?,
            data: row.get::<String>(5).map_err(storage_error)?,
            timestamp: row.get::<String>(6).map_err(storage_error)?,
        })
    }

    // -----------------------------------------------------------------------
    // Design-time event CRUD (Phase 1E)
    // -----------------------------------------------------------------------

    /// Insert a design-time event.
    pub async fn insert_design_time_event(
        &self,
        kind: &str,
        entity_type: &str,
        tenant: &str,
        summary: &str,
        level: Option<&str>,
        passed: Option<bool>,
        step_number: Option<i64>,
        total_steps: Option<i64>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO design_time_events (kind, entity_type, tenant, summary, level, passed, step_number, total_steps) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![kind, entity_type, tenant, summary, level, passed.map(|b| b as i64), step_number, total_steps],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List recent design-time events for a tenant.
    pub async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at \
                 FROM design_time_events \
                 WHERE (?1 IS NULL OR tenant = ?1) \
                 ORDER BY created_at DESC \
                 LIMIT ?2",
                params![tenant, limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(DesignTimeEventRow {
                id: row.get::<i64>(0).map_err(storage_error)?,
                kind: row.get::<String>(1).map_err(storage_error)?,
                entity_type: row.get::<String>(2).map_err(storage_error)?,
                tenant: row.get::<String>(3).map_err(storage_error)?,
                summary: row.get::<String>(4).map_err(storage_error)?,
                level: row.get::<Option<String>>(5).map_err(storage_error)?,
                passed: row
                    .get::<Option<i64>>(6)
                    .map_err(storage_error)?
                    .map(|v| v != 0),
                step_number: row.get::<Option<i64>>(7).map_err(storage_error)?,
                total_steps: row.get::<Option<i64>>(8).map_err(storage_error)?,
                created_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Decision query methods (Phase 1F)
    // -----------------------------------------------------------------------

    /// Query decisions for a specific tenant with optional status filter.
    pub async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
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
    pub async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
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
    pub async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
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

    /// Upsert tenant-level cross-entity constraint definitions.
    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at)
             VALUES (?1, ?2, 1, datetime('now'))
             ON CONFLICT (tenant) DO UPDATE SET
                 cross_invariants_toml = excluded.cross_invariants_toml,
                 version = tenant_constraints.version + 1,
                 updated_at = datetime('now')",
            params![tenant, cross_invariants_toml],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Delete tenant-level cross-entity constraint definitions.
    pub async fn delete_tenant_constraints(&self, tenant: &str) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM tenant_constraints WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all tenant-level cross-entity constraint definitions.
    pub async fn load_tenant_constraints(
        &self,
    ) -> Result<Vec<TursoTenantConstraintRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, cross_invariants_toml, version, updated_at \
                 FROM tenant_constraints \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoTenantConstraintRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                cross_invariants_toml: row.get::<String>(1).map_err(storage_error)?,
                version: row.get::<i64>(2).map_err(storage_error)? as i32,
                updated_at: row.get::<String>(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Upsert a WASM module binary for a tenant.
    ///
    /// If the module already exists, its version is incremented and the binary
    /// is replaced. Returns the SHA-256 hash of the stored module.
    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, datetime('now'))
             ON CONFLICT (tenant, module_name) DO UPDATE SET
                 wasm_bytes = excluded.wasm_bytes,
                 sha256_hash = excluded.sha256_hash,
                 version = wasm_modules.version + 1,
                 size_bytes = excluded.size_bytes,
                 updated_at = datetime('now')",
            params![tenant, module_name, wasm_bytes.to_vec(), sha256_hash, wasm_bytes.len() as i64],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load a WASM module by tenant and name.
    pub async fn load_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<Option<TursoWasmModuleRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Ok(Some(TursoWasmModuleRow {
            tenant: row.get::<String>(0).map_err(storage_error)?,
            module_name: row.get::<String>(1).map_err(storage_error)?,
            wasm_bytes: row.get::<Vec<u8>>(2).map_err(storage_error)?,
            sha256_hash: row.get::<String>(3).map_err(storage_error)?,
            version: row.get::<i64>(4).map_err(storage_error)? as i32,
            size_bytes: row.get::<i64>(5).map_err(storage_error)? as i32,
            updated_at: row.get::<String>(6).map_err(storage_error)?,
        }))
    }

    /// Load all WASM modules for a tenant.
    pub async fn load_all_wasm_modules(
        &self,
        tenant: &str,
    ) -> Result<Vec<TursoWasmModuleRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 WHERE tenant = ?1 \
                 ORDER BY module_name",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoWasmModuleRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                module_name: row.get::<String>(1).map_err(storage_error)?,
                wasm_bytes: row.get::<Vec<u8>>(2).map_err(storage_error)?,
                sha256_hash: row.get::<String>(3).map_err(storage_error)?,
                version: row.get::<i64>(4).map_err(storage_error)? as i32,
                size_bytes: row.get::<i64>(5).map_err(storage_error)? as i32,
                updated_at: row.get::<String>(6).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Load all WASM modules across all tenants (for startup recovery).
    pub async fn load_wasm_modules_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 ORDER BY tenant, module_name",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoWasmModuleRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                module_name: row.get::<String>(1).map_err(storage_error)?,
                wasm_bytes: row.get::<Vec<u8>>(2).map_err(storage_error)?,
                sha256_hash: row.get::<String>(3).map_err(storage_error)?,
                version: row.get::<i64>(4).map_err(storage_error)? as i32,
                size_bytes: row.get::<i64>(5).map_err(storage_error)? as i32,
                updated_at: row.get::<String>(6).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Persist a WASM invocation log entry.
    pub async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        let success_val: i64 = if entry.success { 1 } else { 0 };
        let duration_val: i64 = entry.duration_ms as i64;
        conn.execute(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.tenant,
                entry.entity_type,
                entry.entity_id,
                entry.module_name,
                entry.trigger_action,
                entry.callback_action,
                success_val,
                entry.error,
                duration_val,
                entry.created_at
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load recent WASM invocation log entries (newest first, up to `limit`).
    pub async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, module_name, trigger_action, \
                        callback_action, success, error, duration_ms, created_at \
                 FROM wasm_invocation_logs \
                 ORDER BY created_at DESC \
                 LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoWasmInvocationRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                entity_id: row.get::<String>(2).map_err(storage_error)?,
                module_name: row.get::<String>(3).map_err(storage_error)?,
                trigger_action: row.get::<String>(4).map_err(storage_error)?,
                callback_action: row.get::<Option<String>>(5).map_err(storage_error)?,
                success: row.get::<i64>(6).map_err(storage_error)? != 0,
                error: row.get::<Option<String>>(7).map_err(storage_error)?,
                duration_ms: row.get::<i64>(8).map_err(storage_error)? as u64,
                created_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Delete a WASM module.
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "DELETE FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Obtain a connection handle to the underlying database.
    ///
    /// `Database::connect()` returns a lightweight handle, **not** a fresh TCP
    /// connection each time:
    /// - **Local SQLite** (`file:` URLs): a handle to the same underlying
    ///   database file — no network overhead.
    /// - **Remote Turso** (`libsql://` URLs): a handle drawn from an internal
    ///   HTTP/gRPC connection pool managed by the `libsql` crate.
    ///
    /// It is safe (and cheap) to call this at the start of every method.
    fn connection(&self) -> Result<libsql::Connection, PersistenceError> {
        self.db.connect().map_err(storage_error)
    }

    /// Upsert a pending decision (insert or update).
    pub async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO pending_decisions (id, tenant, status, data, updated_at)              VALUES (?1, ?2, ?3, ?4, datetime('now'))              ON CONFLICT(id) DO UPDATE SET status = ?3, data = ?4, updated_at = datetime('now')",
            params![id, tenant, status, data_json],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all pending decisions (newest first, up to limit).
    pub async fn load_pending_decisions(
        &self,
        limit: i64,
    ) -> Result<Vec<String>, PersistenceError> {
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
    pub async fn upsert_tenant_policy(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_policies (tenant, policy_text, updated_at)              VALUES (?1, ?2, datetime('now'))              ON CONFLICT(tenant) DO UPDATE SET policy_text = ?2, updated_at = datetime('now')",
            params![tenant, policy_text],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all tenant Cedar policies.
    pub async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, PersistenceError> {
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

impl EventStore for TursoEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let mut rows = tx
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let current_seq = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
            None => 0,
        };
        drop(rows);

        if current_seq != expected_sequence {
            let _ = tx.rollback().await;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let payload_json = serde_json::to_string(&event.payload)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let metadata_json = serde_json::to_string(&event.metadata)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            let insert_result = tx
                .execute(
                    "INSERT INTO events
                     (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        new_seq as i64,
                        event.event_type.as_str(),
                        payload_json,
                        metadata_json
                    ],
                )
                .await;

            if let Err(e) = insert_result {
                let msg = e.to_string();
                let _ = tx.rollback().await;
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(new_seq)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
                 ORDER BY sequence_nr ASC",
                params![tenant, entity_type, entity_id, from_sequence as i64],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let seq = row.get::<i64>(0).map_err(storage_error)? as u64;
            let event_type = row.get::<String>(1).map_err(storage_error)?;
            let payload_json = row.get::<String>(2).map_err(storage_error)?;
            let metadata_json = row.get::<Option<String>>(3).map_err(storage_error)?;

            let payload = serde_json::from_str(&payload_json)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let metadata_raw = metadata_json.ok_or_else(|| {
                PersistenceError::Serialization("missing event metadata".to_string())
            })?;
            let metadata: EventMetadata = serde_json::from_str(&metadata_raw)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            out.push(PersistenceEnvelope {
                sequence_nr: seq,
                event_type,
                payload,
                metadata,
            });
        }

        Ok(out)
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        conn.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id)
             DO UPDATE SET
                sequence_nr = excluded.sequence_nr,
                snapshot = excluded.snapshot,
                created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, snapshot
                 FROM snapshots
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ORDER BY sequence_nr DESC
                 LIMIT 1",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        let sequence_nr = row.get::<i64>(0).map_err(storage_error)? as u64;
        let snapshot = row.get::<Vec<u8>>(1).map_err(storage_error)?;
        Ok(Some((sequence_nr, snapshot)))
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_type, entity_id
                 FROM events
                 WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type = row.get::<String>(0).map_err(storage_error)?;
            let entity_id = row.get::<String>(1).map_err(storage_error)?;
            out.push((entity_type, entity_id));
        }
        Ok(out)
    }
}

/// Row returned by [`TursoEventStore::load_specs()`].
#[derive(Debug, Clone)]
pub struct TursoSpecRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// IOA TOML source.
    pub ioa_source: String,
    /// CSDL XML (may be absent for old rows).
    pub csdl_xml: Option<String>,
    /// Verification status string (pending/running/passed/failed/partial).
    pub verification_status: String,
    /// Whether the spec has been verified.
    pub verified: bool,
    /// Number of verification levels that passed.
    pub levels_passed: Option<i32>,
    /// Total number of verification levels.
    pub levels_total: Option<i32>,
    /// Serialized verification result JSON.
    pub verification_result: Option<String>,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}

/// Row returned by trajectory queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TursoTrajectoryRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Action name.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Status before the action.
    pub from_status: Option<String>,
    /// Status after the action.
    pub to_status: Option<String>,
    /// Error description (for failed intents).
    pub error: Option<String>,
    /// Agent identity that performed the action.
    pub agent_id: Option<String>,
    /// Session the action belonged to.
    pub session_id: Option<String>,
    /// Whether this was an authorization denial.
    pub authz_denied: Option<bool>,
    /// Denied resource identifier.
    pub denied_resource: Option<String>,
    /// WASM module involved in the denial.
    pub denied_module: Option<String>,
    /// Source: "Entity", "Platform", "Authz".
    pub source: Option<String>,
    /// Whether the action is governed by a spec.
    pub spec_governed: Option<bool>,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Aggregated trajectory statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrajectoryStats {
    /// Total trajectory count.
    pub total: u64,
    /// Number of successful actions.
    pub success_count: u64,
    /// Number of failed actions.
    pub error_count: u64,
    /// Success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// Per-action breakdown.
    pub by_action: std::collections::BTreeMap<String, ActionStats>,
    /// Recent failed intents.
    pub failed_intents: Vec<TursoTrajectoryRow>,
}

/// Per-action statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActionStats {
    /// Total actions.
    pub total: u64,
    /// Successful actions.
    pub success: u64,
    /// Failed actions.
    pub error: u64,
}

/// Agent summary aggregated from trajectories.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentSummary {
    /// Agent identifier.
    pub agent_id: String,
    /// Total actions performed by this agent.
    pub total_actions: u64,
    /// Successful actions.
    pub success_count: u64,
    /// Failed actions.
    pub error_count: u64,
    /// Authorization denials.
    pub denial_count: u64,
    /// Success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// Most recent activity timestamp.
    pub last_active_at: String,
}

/// Row returned by feature request queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeatureRequestRow {
    /// Feature request ID.
    pub id: String,
    /// Category label.
    pub category: String,
    /// Description of the feature request.
    pub description: String,
    /// Number of trajectory references.
    pub frequency: i64,
    /// JSON array of trajectory reference IDs.
    pub trajectory_refs: String,
    /// Disposition: Open, Acknowledged, Planned, WontFix, Resolved.
    pub disposition: String,
    /// Developer notes.
    pub developer_notes: Option<String>,
    /// ISO-8601 created timestamp.
    pub created_at: String,
    /// ISO-8601 updated timestamp.
    pub updated_at: String,
}

/// Row returned by evolution record queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvolutionRecordRow {
    /// Record ID.
    pub id: String,
    /// Record type: Observation, Problem, Analysis, Decision, Insight.
    pub record_type: String,
    /// Status: Open, Resolved, Superseded, Rejected.
    pub status: String,
    /// Creator identity.
    pub created_by: String,
    /// ID of the parent record this was derived from.
    pub derived_from: Option<String>,
    /// Full record data as JSON.
    pub data: String,
    /// ISO-8601 timestamp.
    pub timestamp: String,
}

/// Row returned by design-time event queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignTimeEventRow {
    /// Auto-increment ID.
    pub id: i64,
    /// Event kind.
    pub kind: String,
    /// Entity type.
    pub entity_type: String,
    /// Tenant.
    pub tenant: String,
    /// Human-readable summary.
    pub summary: String,
    /// Verification level name.
    pub level: Option<String>,
    /// Whether this level passed.
    pub passed: Option<bool>,
    /// Step number in the workflow.
    pub step_number: Option<i64>,
    /// Total steps in the workflow.
    pub total_steps: Option<i64>,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Row returned by WASM module queries.
#[derive(Debug, Clone)]
pub struct TursoWasmModuleRow {
    /// Tenant name.
    pub tenant: String,
    /// Module name.
    pub module_name: String,
    /// Raw WASM binary.
    pub wasm_bytes: Vec<u8>,
    /// SHA-256 hash of the WASM binary.
    pub sha256_hash: String,
    /// Monotonic version counter.
    pub version: i32,
    /// Module size in bytes.
    pub size_bytes: i32,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}

/// Row returned by WASM invocation log queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TursoWasmInvocationRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type that triggered the invocation.
    pub entity_type: String,
    /// Entity ID that triggered the invocation.
    pub entity_id: String,
    /// WASM module name invoked.
    pub module_name: String,
    /// Action that triggered the integration.
    pub trigger_action: String,
    /// Callback action dispatched (if any).
    pub callback_action: Option<String>,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Error description (for failures).
    pub error: Option<String>,
    /// Invocation duration in milliseconds.
    pub duration_ms: u64,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Row returned by [`TursoEventStore::load_tenant_constraints()`].
#[derive(Debug, Clone)]
pub struct TursoTenantConstraintRow {
    /// Tenant name.
    pub tenant: String,
    /// Raw `cross-invariants.toml` source.
    pub cross_invariants_toml: String,
    /// Monotonic version counter.
    pub version: i32,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "store-test".to_string(),
            },
        }
    }

    fn sqlite_test_url(test_name: &str) -> String {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "temper-store-turso-{test_name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        format!("file:{}", path.display())
    }

    async fn make_store(test_name: &str) -> TursoEventStore {
        TursoEventStore::new(&sqlite_test_url(test_name), None)
            .await
            .expect("create store")
    }

    #[tokio::test]
    async fn append_and_read_events_roundtrip() {
        let store = make_store("append-read").await;
        let persistence_id = "tenant-a:Order:ord-1";

        let new_seq = store
            .append(
                persistence_id,
                0,
                &[
                    test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                    test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
                ],
            )
            .await
            .unwrap();

        assert_eq!(new_seq, 2);

        let events = store.read_events(persistence_id, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence_nr, 1);
        assert_eq!(events[1].sequence_nr, 2);
        assert_eq!(events[0].event_type, "OrderCreated");
        assert_eq!(events[1].event_type, "OrderApproved");
    }

    #[tokio::test]
    async fn append_with_wrong_sequence_fails_with_concurrency_violation() {
        let store = make_store("concurrency").await;
        let persistence_id = "tenant-a:Order:ord-2";

        store
            .append(
                persistence_id,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-2" }),
                )],
            )
            .await
            .unwrap();

        let err = store
            .append(
                persistence_id,
                0,
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({ "step": 2 }),
                )],
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            PersistenceError::ConcurrencyViolation {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn snapshot_save_and_load_roundtrip() {
        let store = make_store("snapshot").await;
        let persistence_id = "tenant-a:Order:ord-3";

        store
            .save_snapshot(persistence_id, 5, b"{\"status\":\"created\"}")
            .await
            .unwrap();

        let snapshot = store.load_snapshot(persistence_id).await.unwrap();
        assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

        store
            .save_snapshot(persistence_id, 8, b"{\"status\":\"shipped\"}")
            .await
            .unwrap();

        let updated = store.load_snapshot(persistence_id).await.unwrap();
        assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
    }

    #[tokio::test]
    async fn list_entity_ids_returns_distinct_pairs() {
        let store = make_store("entity-list").await;

        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());

        let order_1 = format!("{tenant_a}:Order:ord-1");
        let order_2 = format!("{tenant_a}:Order:ord-2");
        let task_1 = format!("{tenant_a}:Task:task-1");
        let other_tenant = format!("{tenant_b}:Order:ord-9");

        store
            .append(
                &order_1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &order_1,
                1,
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({ "step": 2 }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &order_2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-2" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &task_1,
                0,
                &[test_envelope(
                    "TaskCreated",
                    serde_json::json!({ "id": "task-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &other_tenant,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-9" }),
                )],
            )
            .await
            .unwrap();

        let mut entities = store.list_entity_ids(&tenant_a).await.unwrap();
        entities.sort();

        assert_eq!(
            entities,
            vec![
                ("Order".to_string(), "ord-1".to_string()),
                ("Order".to_string(), "ord-2".to_string()),
                ("Task".to_string(), "task-1".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let store = make_store("migrate-idempotent").await;

        store.migrate().await.unwrap();
        store.migrate().await.unwrap();
    }
}
