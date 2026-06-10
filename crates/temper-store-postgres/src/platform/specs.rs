//! Spec storage, verification cache, tenant constraints, and installed apps.

use std::collections::BTreeMap;

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_optional_json, parse_optional_rfc3339, storage_error};
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

impl PostgresEventStore {
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
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
        let rows = crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query!("DELETE FROM specs WHERE tenant = $1 AND entity_type = $2")
            .bind(tenant)
            .bind(entity_type)
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            "UPDATE specs SET committed = true, updated_at = now() WHERE tenant = $1"
        )
        .bind(tenant)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let result = crate::dbm::postgres_query!("DELETE FROM specs WHERE committed = false")
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(result.rows_affected() as usize)
    }

    pub async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, PersistenceError> {
        let rows: Vec<(String, String, bool)> = crate::dbm::postgres_query_as!(
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
        crate::dbm::postgres_query!(
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

    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query_scalar!(
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
        crate::dbm::postgres_query!(
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
        let row = crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query_as!(
            "SELECT tenant, app_name FROM tenant_installed_apps ORDER BY tenant, app_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }
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
