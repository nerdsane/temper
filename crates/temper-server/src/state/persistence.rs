//! Persistence methods for ServerState (Postgres, Turso, Redis backends).

use sqlx::types::Json;
use temper_runtime::scheduler::sim_now;
use temper_store_turso::{TursoSpecVerificationUpdate, TursoTrajectoryInsert};

use super::trajectory::TrajectoryEntry;
use super::{DESIGN_TIME_LOG_CAPACITY, DesignTimeEvent, ServerState};
use crate::registry::EntityVerificationResult;

impl ServerState {
    /// Upsert a WASM module in the persistence backend (Postgres or Turso).
    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
    ) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Postgres path.
        if let Some(pool) = store.postgres_pool() {
            sqlx::query(
                "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at) \
                 VALUES ($1, $2, $3, $4, 1, $5, now()) \
                 ON CONFLICT (tenant, module_name) DO UPDATE SET \
                     wasm_bytes = EXCLUDED.wasm_bytes, \
                     sha256_hash = EXCLUDED.sha256_hash, \
                     version = wasm_modules.version + 1, \
                     size_bytes = EXCLUDED.size_bytes, \
                     updated_at = now()",
            )
            .bind(tenant)
            .bind(module_name)
            .bind(wasm_bytes)
            .bind(sha256_hash)
            .bind(wasm_bytes.len() as i32)
            .execute(pool)
            .await
            .map_err(|e| format!("failed to upsert WASM module {tenant}/{module_name}: {e}"))?;
            return Ok(());
        }

        // Turso path.
        if let Some(turso) = store.turso_store() {
            turso
                .upsert_wasm_module(tenant, module_name, wasm_bytes, sha256_hash)
                .await
                .map_err(|e| {
                    format!("failed to upsert WASM module {tenant}/{module_name} in turso: {e}")
                })?;
            return Ok(());
        }

        Ok(())
    }

    /// Delete a WASM module from persistence.
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(false);
        };

        if let Some(pool) = store.postgres_pool() {
            let result =
                sqlx::query("DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2")
                    .bind(tenant)
                    .bind(module_name)
                    .execute(pool)
                    .await
                    .map_err(|e| {
                        format!("failed to delete WASM module {tenant}/{module_name}: {e}")
                    })?;
            return Ok(result.rows_affected() > 0);
        }

        if let Some(turso) = store.turso_store() {
            return turso
                .delete_wasm_module(tenant, module_name)
                .await
                .map_err(|e| {
                    format!("failed to delete WASM module {tenant}/{module_name} in turso: {e}")
                });
        }

        Ok(false)
    }

    /// Upsert a spec source into the persistence backend (Postgres or Turso).
    pub async fn upsert_spec_source(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Try Postgres first.
        if let Some(pool) = store.postgres_pool() {
            sqlx::query(
                "INSERT INTO specs \
                 (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at) \
                 VALUES ($1, $2, $3, $4, 1, false, 'pending', now()) \
                 ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                     ioa_source = EXCLUDED.ioa_source, \
                     csdl_xml = EXCLUDED.csdl_xml, \
                     version = specs.version + 1, \
                     verified = false, \
                     verification_status = 'pending', \
                     levels_passed = NULL, \
                     levels_total = NULL, \
                     verification_result = NULL, \
                     updated_at = now()",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(ioa_source)
            .bind(csdl_xml)
            .execute(pool)
            .await
            .map_err(|e| format!("failed to upsert spec {tenant}/{entity_type} in postgres: {e}"))?;
            return Ok(());
        }

        // Fall back to Turso.
        if let Some(turso) = store.turso_store() {
            turso
                .upsert_spec(tenant, entity_type, ioa_source, csdl_xml)
                .await
                .map_err(|e| {
                    format!("failed to upsert spec {tenant}/{entity_type} in turso: {e}")
                })?;
            return Ok(());
        }

        // Redis: not suited for relational metadata — silently skip.
        Ok(())
    }

    /// Upsert tenant-level cross-invariant definitions.
    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: Option<&str>,
    ) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Postgres path.
        if let Some(pool) = store.postgres_pool() {
            if let Some(source) = cross_invariants_toml {
                sqlx::query(
                    "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
                     VALUES ($1, $2, 1, now()) \
                     ON CONFLICT (tenant) DO UPDATE SET \
                        cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                        version = tenant_constraints.version + 1, \
                        updated_at = now()",
                )
                .bind(tenant)
                .bind(source)
                .execute(pool)
                .await
                .map_err(|e| format!("failed to upsert tenant constraints for {tenant}: {e}"))?;
            } else {
                sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
                    .bind(tenant)
                    .execute(pool)
                    .await
                    .map_err(|e| format!("failed to clear tenant constraints for {tenant}: {e}"))?;
            }
            return Ok(());
        }

        // Turso path.
        if let Some(turso) = store.turso_store() {
            if let Some(source) = cross_invariants_toml {
                turso
                    .upsert_tenant_constraints(tenant, source)
                    .await
                    .map_err(|e| {
                        format!("failed to upsert tenant constraints for {tenant} in turso: {e}")
                    })?;
            } else {
                turso.delete_tenant_constraints(tenant).await.map_err(|e| {
                    format!("failed to clear tenant constraints for {tenant} in turso: {e}")
                })?;
            }
            return Ok(());
        }

        // Redis: not suited for relational metadata — silently skip.
        Ok(())
    }

    /// Persist verification summary for a spec (Postgres, Turso, or skip for Redis).
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        status: &str,
        result: Option<&EntityVerificationResult>,
    ) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        let (verified, levels_passed, levels_total, verification_result) = match result {
            Some(r) => {
                let passed = r.levels.iter().filter(|l| l.passed).count() as i32;
                let total = r.levels.len() as i32;
                let as_json = serde_json::to_value(r).ok();
                (r.all_passed, Some(passed), Some(total), as_json)
            }
            None => (false, None, None, None),
        };

        // Try Postgres first.
        if let Some(pool) = store.postgres_pool() {
            sqlx::query(
                "UPDATE specs SET \
                     verified = $3, \
                     verification_status = $4, \
                     levels_passed = $5, \
                     levels_total = $6, \
                     verification_result = $7, \
                     updated_at = now() \
                 WHERE tenant = $1 AND entity_type = $2",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(verified)
            .bind(status)
            .bind(levels_passed)
            .bind(levels_total)
            .bind(verification_result.map(Json))
            .execute(pool)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist spec verification status for {tenant}/{entity_type} ({status}): {e}"
                )
            })?;
            return Ok(());
        }

        // Fall back to Turso.
        if let Some(turso) = store.turso_store() {
            let result_json = verification_result
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok());
            turso
                .persist_spec_verification(
                    tenant,
                    entity_type,
                    TursoSpecVerificationUpdate {
                        status,
                        verified,
                        levels_passed,
                        levels_total,
                        verification_result_json: result_json.as_deref(),
                    },
                )
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist spec verification status for {tenant}/{entity_type} ({status}) in turso: {e}"
                    )
                })?;
            return Ok(());
        }

        // Redis: not suited for relational metadata — silently skip.
        Ok(())
    }

    /// Append to in-memory design-time log with bounded capacity.
    pub fn push_design_time_event(&self, event: DesignTimeEvent) {
        if let Ok(mut log) = self.design_time_log.write() {
            if log.len() >= DESIGN_TIME_LOG_CAPACITY {
                // Keep the newest events; evict oldest one.
                let _ = log.remove(0);
            }
            log.push(event);
        }
    }

    /// Broadcast and persist a design-time event.
    pub async fn emit_design_time_event(&self, event: DesignTimeEvent) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            let _ = self.design_time_tx.send(event.clone());
            self.push_design_time_event(event);
            return Ok(());
        };
        let created_at = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| sim_now());
        sqlx::query(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&event.kind)
        .bind(&event.entity_type)
        .bind(&event.tenant)
        .bind(&event.summary)
        .bind(event.level.as_deref())
        .bind(event.passed)
        .bind(event.step_number.map(i16::from))
        .bind(event.total_steps.map(i16::from))
        .bind(created_at)
        .execute(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to persist design-time event {} for {}/{}: {e}",
                event.kind, event.tenant, event.entity_type
            )
        })?;
        let _ = self.design_time_tx.send(event.clone());
        self.push_design_time_event(event);
        Ok(())
    }

    /// Persist a trajectory entry (Postgres, Turso, or Redis).
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Try Postgres first.
        if let Some(pool) = store.postgres_pool() {
            let created_at = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| sim_now());
            sqlx::query(
                "INSERT INTO trajectories \
                 (tenant, entity_type, entity_id, action, success, from_status, to_status, error, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&entry.tenant)
            .bind(&entry.entity_type)
            .bind(&entry.entity_id)
            .bind(&entry.action)
            .bind(entry.success)
            .bind(entry.from_status.as_deref())
            .bind(entry.to_status.as_deref())
            .bind(entry.error.as_deref())
            .bind(created_at)
            .execute(pool)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist trajectory entry for {}/{}/{} action {}: {e}",
                    entry.tenant, entry.entity_type, entry.entity_id, entry.action
                )
            })?;
            return Ok(());
        }

        // Fall back to Turso.
        if let Some(turso) = store.turso_store() {
            turso
                .persist_trajectory(TursoTrajectoryInsert {
                    tenant: &entry.tenant,
                    entity_type: &entry.entity_type,
                    entity_id: &entry.entity_id,
                    action: &entry.action,
                    success: entry.success,
                    from_status: entry.from_status.as_deref(),
                    to_status: entry.to_status.as_deref(),
                    error: entry.error.as_deref(),
                    created_at: &entry.timestamp,
                })
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in turso: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                })?;
            return Ok(());
        }

        // Fall back to Redis (capped list).
        if let Some(redis) = store.redis_store() {
            let entry_json = serde_json::to_string(entry)
                .map_err(|e| format!("failed to serialize trajectory entry: {e}"))?;
            redis
                .persist_trajectory(
                    &entry.tenant,
                    &entry_json,
                    super::TRAJECTORY_LOG_CAPACITY as i64,
                )
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in redis: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                })?;
        }

        Ok(())
    }
}
