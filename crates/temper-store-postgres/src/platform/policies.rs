//! Cedar policy storage, policy denial patterns, and pending decisions.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_json, parse_rfc3339, storage_error};
use crate::PostgresEventStore;

const DISTINCT_RESOURCE_IDS_BUDGET: usize = 100;

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresPolicyDenialPatternRow {
    pub tenant: String,
    pub agent_type: Option<String>,
    pub action: String,
    pub resource_type: String,
    pub count: i64,
    pub first_seen: String,
    pub last_seen: String,
    pub distinct_resource_ids_json: String,
}

impl PostgresEventStore {
    pub async fn upsert_tenant_policy(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query_as!(
            "SELECT tenant, policy_text FROM tenant_policies ORDER BY tenant"
        )
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
        let existing_hash: Option<String> = crate::dbm::postgres_query_scalar!(
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

        crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query!(
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
        let result = crate::dbm::postgres_query!(
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
        let result = crate::dbm::postgres_query!(
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
        crate::dbm::postgres_query!("DELETE FROM policies WHERE tenant = $1 AND policy_id = $2")
            .bind(tenant)
            .bind(policy_id)
            .execute(self.pool())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(data)?;
        crate::dbm::postgres_query!(
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
        let rows: Vec<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM pending_decisions ORDER BY updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(|v| v.to_string()).collect())
    }

    pub async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        let agent_type_key = agent_type.unwrap_or("");
        let timestamp = parse_rfc3339(timestamp)?;
        let existing = crate::dbm::postgres_query!(
            "SELECT count, first_seen, last_seen, distinct_resource_ids_json \
             FROM policy_denial_patterns \
             WHERE tenant = $1 AND agent_type = $2 AND action = $3 AND resource_type = $4",
        )
        .bind(tenant)
        .bind(agent_type_key)
        .bind(action)
        .bind(resource_type)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;

        let mut count = 1_i64;
        let mut first_seen = timestamp;
        let mut last_seen = timestamp;
        let mut distinct_resource_ids = BTreeSet::new();
        if let Some(row) = existing {
            count = row.get::<i64, _>("count") + 1;
            first_seen = row.get("first_seen");
            let existing_last_seen: chrono::DateTime<chrono::Utc> = row.get("last_seen");
            last_seen = existing_last_seen.max(timestamp);
            let ids: serde_json::Value = row.get("distinct_resource_ids_json");
            if let Ok(values) = serde_json::from_value::<Vec<String>>(ids) {
                distinct_resource_ids.extend(values);
            }
        }
        distinct_resource_ids.insert(resource_id.to_string());
        while distinct_resource_ids.len() > DISTINCT_RESOURCE_IDS_BUDGET {
            if let Some(oldest) = distinct_resource_ids.iter().next().cloned() {
                distinct_resource_ids.remove(&oldest);
            } else {
                break;
            }
        }
        let ids_json = serde_json::to_value(distinct_resource_ids.into_iter().collect::<Vec<_>>())
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO policy_denial_patterns \
             (tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (tenant, agent_type, action, resource_type) DO UPDATE SET \
                 count = EXCLUDED.count, first_seen = EXCLUDED.first_seen, last_seen = EXCLUDED.last_seen, \
                 distinct_resource_ids_json = EXCLUDED.distinct_resource_ids_json",
        )
        .bind(tenant)
        .bind(agent_type_key)
        .bind(action)
        .bind(resource_type)
        .bind(count)
        .bind(first_seen)
        .bind(last_seen)
        .bind(ids_json)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresPolicyDenialPatternRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json \
             FROM policy_denial_patterns \
             WHERE tenant = $1 \
             ORDER BY last_seen DESC, count DESC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_policy_denial_pattern).collect())
    }

    pub async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM pending_decisions \
             WHERE tenant = $1 AND ($2::text IS NULL OR status = $2) \
             ORDER BY created_at DESC",
        )
        .bind(tenant)
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(|value| value.to_string()).collect())
    }

    pub async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM pending_decisions \
             WHERE ($1::text IS NULL OR status = $1) \
             ORDER BY created_at DESC",
        )
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(|value| value.to_string()).collect())
    }

    pub async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        let row: Option<serde_json::Value> =
            crate::dbm::postgres_query_scalar!("SELECT data FROM pending_decisions WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await
                .map_err(storage_error)?;
        Ok(row.map(|value| value.to_string()))
    }
}

fn compute_policy_hash(cedar_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cedar_text.as_bytes());
    format!("{:x}", hasher.finalize())
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

fn row_to_policy_denial_pattern(row: sqlx::postgres::PgRow) -> PostgresPolicyDenialPatternRow {
    let first_seen: chrono::DateTime<chrono::Utc> = row.get("first_seen");
    let last_seen: chrono::DateTime<chrono::Utc> = row.get("last_seen");
    let agent_type_raw: String = row.get("agent_type");
    let distinct_resource_ids_json: serde_json::Value = row.get("distinct_resource_ids_json");
    PostgresPolicyDenialPatternRow {
        tenant: row.get("tenant"),
        agent_type: if agent_type_raw.is_empty() {
            None
        } else {
            Some(agent_type_raw)
        },
        action: row.get("action"),
        resource_type: row.get("resource_type"),
        count: row.get("count"),
        first_seen: first_seen.to_rfc3339(),
        last_seen: last_seen.to_rfc3339(),
        distinct_resource_ids_json: distinct_resource_ids_json.to_string(),
    }
}
