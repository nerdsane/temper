//! PostgreSQL platform-store methods.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;

#[derive(Clone, Copy, Debug)]
pub struct PostgresSpecVerificationUpdate<'a> {
    pub status: &'a str,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result_json: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct PostgresSpecRow {
    pub tenant: String,
    pub entity_type: String,
    pub ioa_source: String,
    pub csdl_xml: Option<String>,
    pub verification_status: String,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result: Option<String>,
    pub content_hash: Option<String>,
    pub updated_at: String,
    pub committed: bool,
}

#[derive(Debug, Clone)]
pub struct PostgresInstalledAppRow {
    pub tenant: String,
    pub app_name: String,
    pub app_version: String,
    pub bundle_digest: String,
    pub spec_digest: String,
    pub policy_digest: String,
    pub wasm_digest: String,
    pub content_digest: String,
    pub seed_digest: String,
    pub installed_at: String,
    pub last_reconciled_at: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct PostgresWasmModuleRow {
    pub tenant: String,
    pub module_name: String,
    pub wasm_bytes: Vec<u8>,
    pub sha256_hash: String,
}

#[derive(Debug, Clone)]
pub struct PostgresPolicyRow {
    pub tenant: String,
    pub policy_id: String,
    pub cedar_text: String,
    pub policy_hash: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresTrajectoryInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub success: bool,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub error: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub authz_denied: Option<bool>,
    pub denied_resource: Option<&'a str>,
    pub denied_module: Option<&'a str>,
    pub source: Option<&'a str>,
    pub spec_governed: Option<bool>,
    pub created_at: &'a str,
    pub request_body: Option<&'a str>,
    pub intent: Option<&'a str>,
    pub matched_policy_ids: Option<&'a str>,
}

impl PostgresEventStore {
    pub async fn persist_trajectory(
        &self,
        entry: PostgresTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let created_at = parse_rfc3339(entry.created_at)?;
        let request_body = parse_optional_json(entry.request_body)?;
        let matched_policy_ids = parse_optional_json(entry.matched_policy_ids)?;
        sqlx::query(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
              agent_id, session_id, authz_denied, denied_resource, denied_module, source, \
              spec_governed, created_at, request_body, intent, matched_policy_ids) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
        )
        .bind(entry.tenant)
        .bind(entry.entity_type)
        .bind(entry.entity_id)
        .bind(entry.action)
        .bind(entry.success)
        .bind(entry.from_status)
        .bind(entry.to_status)
        .bind(entry.error)
        .bind(entry.agent_id)
        .bind(entry.session_id)
        .bind(entry.authz_denied)
        .bind(entry.denied_resource)
        .bind(entry.denied_module)
        .bind(entry.source)
        .bind(entry.spec_governed)
        .bind(created_at)
        .bind(request_body)
        .bind(entry.intent)
        .bind(matched_policy_ids)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let projection_hash = json_hash(fields);
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query(
            "INSERT INTO entity_catalog \
             (tenant, entity_type, entity_id, status, fields, sequence_nr, projection_version, projection_hash, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 2, $7, now()) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 status = EXCLUDED.status, fields = EXCLUDED.fields, sequence_nr = EXCLUDED.sequence_nr, \
                 projection_version = EXCLUDED.projection_version, projection_hash = EXCLUDED.projection_hash, updated_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(status)
        .bind(fields)
        .bind(sequence_nr as i64)
        .bind(projection_hash)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;

        sqlx::query("DELETE FROM entity_field_index WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3")
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;

        if let Some(object) = fields.as_object() {
            for (field_name, value) in object {
                if let Some(field_value) = scalar_field_value(value) {
                    sqlx::query(
                        "INSERT INTO entity_field_index \
                         (tenant, entity_type, entity_id, field_name, field_value, status) \
                         VALUES ($1, $2, $3, $4, $5, $6) \
                         ON CONFLICT (tenant, entity_type, entity_id, field_name) DO UPDATE SET \
                             field_value = EXCLUDED.field_value, status = EXCLUDED.status",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(entity_id)
                    .bind(field_name)
                    .bind(field_value)
                    .bind(status)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                }
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query(
            "DELETE FROM entity_catalog WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        sqlx::query("DELETE FROM entity_field_index WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3")
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Vec<String>, PersistenceError> {
        let clause = postgres_placeholders(where_clause, params.len() + 2);
        let sql = format!(
            "SELECT entity_id FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND ({clause}) \
             ORDER BY entity_id"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        query.fetch_all(self.pool()).await.map_err(storage_error)
    }

    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Vec<(String, u64)>, PersistenceError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT tenant, COUNT(*)::bigint FROM entity_catalog GROUP BY tenant ORDER BY tenant",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(tenant, count)| (tenant, count as u64))
            .collect())
    }

    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            "INSERT INTO specs \
             (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, $5, false, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.ioa_source ELSE specs.ioa_source END, \
                 csdl_xml = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.csdl_xml ELSE specs.csdl_xml END, \
                 content_hash = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.content_hash ELSE specs.content_hash END, \
                 committed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.committed END, \
                 version = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN specs.version + 1 ELSE specs.version END, \
                 verified = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.verified END, \
                 verification_status = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN 'pending' ELSE specs.verification_status END, \
                 levels_passed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_passed END, \
                 levels_total = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_total END, \
                 verification_result = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.verification_result END, \
                 updated_at = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN now() ELSE specs.updated_at END",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(csdl_xml)
        .bind(content_hash)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_specs(&self) -> Result<Vec<PostgresSpecRow>, PersistenceError> {
        let rows = sqlx::query(
            "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                    levels_passed, levels_total, verification_result, content_hash, updated_at, committed \
             FROM specs WHERE committed = true ORDER BY tenant, entity_type",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_spec).collect())
    }

    pub async fn delete_spec(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query("DELETE FROM specs WHERE tenant = $1 AND entity_type = $2")
            .bind(tenant)
            .bind(entity_type)
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        sqlx::query("UPDATE specs SET committed = true, updated_at = now() WHERE tenant = $1")
            .bind(tenant)
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let result = sqlx::query("DELETE FROM specs WHERE committed = false")
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(result.rows_affected() as usize)
    }

    pub async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, PersistenceError> {
        let rows: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT entity_type, content_hash, verified FROM specs WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(entity_type, hash, verified)| (entity_type, (hash, verified)))
            .collect())
    }

    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: PostgresSpecVerificationUpdate<'_>,
    ) -> Result<(), PersistenceError> {
        let verification_result = parse_optional_json(update.verification_result_json)?;
        sqlx::query(
            "UPDATE specs SET verification_status = $3, verified = $4, levels_passed = $5, \
             levels_total = $6, verification_result = $7, updated_at = now() \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(update.status)
        .bind(update.verified)
        .bind(update.levels_passed)
        .bind(update.levels_total)
        .bind(verification_result)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn upsert_tenant_policy(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            "INSERT INTO tenant_policies (tenant, policy_text, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (tenant) DO UPDATE SET policy_text = EXCLUDED.policy_text, updated_at = now()",
        )
        .bind(tenant)
        .bind(policy_text)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, PersistenceError> {
        sqlx::query_as("SELECT tenant, policy_text FROM tenant_policies ORDER BY tenant")
            .fetch_all(self.pool())
            .await
            .map_err(storage_error)
    }

    pub async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, PersistenceError> {
        let policy_hash = compute_policy_hash(cedar_text);
        let existing_hash: Option<String> = sqlx::query_scalar(
            "SELECT policy_hash FROM policies WHERE tenant = $1 AND policy_id = $2",
        )
        .bind(tenant)
        .bind(policy_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;

        if existing_hash.as_deref() == Some(policy_hash.as_str()) {
            return Ok(false);
        }

        sqlx::query(
            "INSERT INTO policies \
             (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
             VALUES ($1, $2, $3, $4, now(), $5, true) \
             ON CONFLICT (tenant, policy_id) DO UPDATE SET \
                 cedar_text = EXCLUDED.cedar_text, \
                 policy_hash = EXCLUDED.policy_hash, \
                 created_by = EXCLUDED.created_by, \
                 created_at = now()",
        )
        .bind(tenant)
        .bind(policy_id)
        .bind(cedar_text)
        .bind(&policy_hash)
        .bind(created_by)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;

        Ok(true)
    }

    pub async fn load_policies_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresPolicyRow>, PersistenceError> {
        sqlx::query(
            "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
             FROM policies \
             WHERE tenant = $1 \
             ORDER BY created_at ASC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map(|rows| rows.into_iter().map(row_to_policy).collect())
        .map_err(storage_error)
    }

    pub async fn load_all_policies(&self) -> Result<Vec<PostgresPolicyRow>, PersistenceError> {
        sqlx::query(
            "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
             FROM policies \
             ORDER BY tenant ASC, created_at ASC",
        )
        .fetch_all(self.pool())
        .await
        .map(|rows| rows.into_iter().map(row_to_policy).collect())
        .map_err(storage_error)
    }

    pub async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, PersistenceError> {
        let result = sqlx::query(
            "UPDATE policies SET enabled = $3 \
             WHERE tenant = $1 AND policy_id = $2",
        )
        .bind(tenant)
        .bind(policy_id)
        .bind(enabled)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, PersistenceError> {
        let policy_hash = compute_policy_hash(cedar_text);
        let result = sqlx::query(
            "UPDATE policies \
             SET cedar_text = $3, policy_hash = $4, created_by = $5, created_at = now() \
             WHERE tenant = $1 AND policy_id = $2",
        )
        .bind(tenant)
        .bind(policy_id)
        .bind(cedar_text)
        .bind(&policy_hash)
        .bind(created_by)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_policy(
        &self,
        tenant: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query("DELETE FROM policies WHERE tenant = $1 AND policy_id = $2")
            .bind(tenant)
            .bind(policy_id)
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
             VALUES ($1, $2, 1, now()) \
             ON CONFLICT (tenant) DO UPDATE SET cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                 version = tenant_constraints.version + 1, updated_at = now()",
        )
        .bind(tenant)
        .bind(cross_invariants_toml)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn is_app_installed(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<bool, PersistenceError> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tenant_installed_apps WHERE tenant = $1 AND app_name = $2)",
        )
        .bind(tenant)
        .bind(app_name)
        .fetch_one(self.pool())
        .await
        .map_err(storage_error)
    }

    pub async fn record_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<(), PersistenceError> {
        let record = PostgresInstalledAppRow {
            tenant: tenant.to_string(),
            app_name: app_name.to_string(),
            app_version: String::new(),
            bundle_digest: String::new(),
            spec_digest: String::new(),
            policy_digest: String::new(),
            wasm_digest: String::new(),
            content_digest: String::new(),
            seed_digest: String::new(),
            installed_at: String::new(),
            last_reconciled_at: None,
            status: "installed".to_string(),
        };
        self.record_installed_app_metadata(&record).await
    }

    pub async fn record_installed_app_metadata(
        &self,
        record: &PostgresInstalledAppRow,
    ) -> Result<(), PersistenceError> {
        let last_reconciled_at = parse_optional_rfc3339(record.last_reconciled_at.as_deref())?;
        sqlx::query(
            "INSERT INTO tenant_installed_apps \
             (tenant, app_name, app_version, bundle_digest, spec_digest, policy_digest, wasm_digest, \
              content_digest, seed_digest, installed_at, last_reconciled_at, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), $10, $11) \
             ON CONFLICT (tenant, app_name) DO UPDATE SET \
                 app_version = EXCLUDED.app_version, bundle_digest = EXCLUDED.bundle_digest, \
                 spec_digest = EXCLUDED.spec_digest, policy_digest = EXCLUDED.policy_digest, \
                 wasm_digest = EXCLUDED.wasm_digest, content_digest = EXCLUDED.content_digest, \
                 seed_digest = EXCLUDED.seed_digest, last_reconciled_at = EXCLUDED.last_reconciled_at, status = EXCLUDED.status",
        )
        .bind(&record.tenant)
        .bind(&record.app_name)
        .bind(&record.app_version)
        .bind(&record.bundle_digest)
        .bind(&record.spec_digest)
        .bind(&record.policy_digest)
        .bind(&record.wasm_digest)
        .bind(&record.content_digest)
        .bind(&record.seed_digest)
        .bind(last_reconciled_at)
        .bind(&record.status)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<PostgresInstalledAppRow>, PersistenceError> {
        let row = sqlx::query(
            "SELECT tenant, app_name, app_version, bundle_digest, spec_digest, policy_digest, \
                    wasm_digest, content_digest, seed_digest, installed_at, last_reconciled_at, status \
             FROM tenant_installed_apps WHERE tenant = $1 AND app_name = $2",
        )
        .bind(tenant)
        .bind(app_name)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_installed_app))
    }

    pub async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, PersistenceError> {
        sqlx::query_as(
            "SELECT tenant, app_name FROM tenant_installed_apps ORDER BY tenant, app_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }

    pub async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(data)?;
        sqlx::query(
            "INSERT INTO pending_decisions (id, tenant, status, data, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (id) DO UPDATE SET tenant = EXCLUDED.tenant, status = EXCLUDED.status, \
                 data = EXCLUDED.data, updated_at = now()",
        )
        .bind(id)
        .bind(tenant)
        .bind(status)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_pending_decisions(
        &self,
        limit: i64,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT data FROM pending_decisions ORDER BY updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(|v| v.to_string()).collect())
    }

    pub async fn load_all_wasm_modules(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresWasmModuleRow>, PersistenceError> {
        let rows = sqlx::query(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash \
             FROM wasm_modules WHERE tenant = $1 ORDER BY module_name",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module).collect())
    }

    pub async fn load_wasm_modules_all_tenants(
        &self,
    ) -> Result<Vec<PostgresWasmModuleRow>, PersistenceError> {
        let rows = sqlx::query(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash \
             FROM wasm_modules ORDER BY tenant, module_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module).collect())
    }

    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at) \
             VALUES ($1, $2, $3, $4, 1, $5, now()) \
             ON CONFLICT (tenant, module_name) DO UPDATE SET wasm_bytes = EXCLUDED.wasm_bytes, \
                 sha256_hash = EXCLUDED.sha256_hash, version = wasm_modules.version + 1, \
                 size_bytes = EXCLUDED.size_bytes, updated_at = now()",
        )
        .bind(tenant)
        .bind(name)
        .bind(bytes)
        .bind(hash)
        .bind(bytes.len() as i32)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }
}

fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}

fn parse_json(data: &str) -> Result<serde_json::Value, PersistenceError> {
    serde_json::from_str(data).map_err(|e| PersistenceError::Serialization(e.to_string()))
}

fn parse_optional_json(data: Option<&str>) -> Result<Option<serde_json::Value>, PersistenceError> {
    data.map(parse_json).transpose()
}

fn parse_rfc3339(value: &str) -> Result<chrono::DateTime<chrono::Utc>, PersistenceError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| PersistenceError::Serialization(e.to_string()))
}

fn parse_optional_rfc3339(
    value: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
    value.map(parse_rfc3339).transpose()
}

fn json_hash(value: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn compute_policy_hash(cedar_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cedar_text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn scalar_field_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(v) => Some(v.to_string()),
        serde_json::Value::Number(v) => Some(v.to_string()),
        serde_json::Value::String(v) => Some(v.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

fn postgres_placeholders(sql: &str, max_index: usize) -> String {
    let mut out = sql.to_string();
    for index in (1..=max_index).rev() {
        out = out.replace(&format!("?{index}"), &format!("${index}"));
    }
    out
}

fn row_to_spec(row: sqlx::postgres::PgRow) -> PostgresSpecRow {
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    let verification_result: Option<serde_json::Value> = row.get("verification_result");
    PostgresSpecRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        ioa_source: row.get("ioa_source"),
        csdl_xml: row.get("csdl_xml"),
        verification_status: row.get("verification_status"),
        verified: row.get("verified"),
        levels_passed: row.get("levels_passed"),
        levels_total: row.get("levels_total"),
        verification_result: verification_result.map(|v| v.to_string()),
        content_hash: Some(row.get("content_hash")),
        updated_at: updated_at.to_rfc3339(),
        committed: row.get("committed"),
    }
}

fn row_to_installed_app(row: sqlx::postgres::PgRow) -> PostgresInstalledAppRow {
    let installed_at: chrono::DateTime<chrono::Utc> = row.get("installed_at");
    let last_reconciled_at: Option<chrono::DateTime<chrono::Utc>> = row.get("last_reconciled_at");
    PostgresInstalledAppRow {
        tenant: row.get("tenant"),
        app_name: row.get("app_name"),
        app_version: row.get("app_version"),
        bundle_digest: row.get("bundle_digest"),
        spec_digest: row.get("spec_digest"),
        policy_digest: row.get("policy_digest"),
        wasm_digest: row.get("wasm_digest"),
        content_digest: row.get("content_digest"),
        seed_digest: row.get("seed_digest"),
        installed_at: installed_at.to_rfc3339(),
        last_reconciled_at: last_reconciled_at.map(|dt| dt.to_rfc3339()),
        status: row.get("status"),
    }
}

fn row_to_wasm_module(row: sqlx::postgres::PgRow) -> PostgresWasmModuleRow {
    PostgresWasmModuleRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        wasm_bytes: row.get("wasm_bytes"),
        sha256_hash: row.get("sha256_hash"),
    }
}

fn row_to_policy(row: sqlx::postgres::PgRow) -> PostgresPolicyRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    PostgresPolicyRow {
        tenant: row.get("tenant"),
        policy_id: row.get("policy_id"),
        cedar_text: row.get("cedar_text"),
        policy_hash: row.get("policy_hash"),
        created_at: created_at.to_rfc3339(),
        created_by: row.get("created_by"),
        enabled: row.get("enabled"),
    }
}
