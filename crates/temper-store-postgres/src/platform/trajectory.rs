//! Trajectory persistence and query methods.

use std::collections::BTreeMap;

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_optional_json, parse_rfc3339, storage_error};
use crate::PostgresEventStore;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresTrajectoryRow {
    pub tenant: String,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub success: bool,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub error: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
    pub authz_denied: Option<bool>,
    pub denied_resource: Option<String>,
    pub denied_module: Option<String>,
    pub source: Option<String>,
    pub spec_governed: Option<bool>,
    pub created_at: String,
    pub request_body: Option<String>,
    pub intent: Option<String>,
    pub matched_policy_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresTrajectoryStats {
    pub total: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub success_rate: f64,
    pub by_action: BTreeMap<String, PostgresActionStats>,
    pub failed_intents: Vec<PostgresTrajectoryRow>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresActionStats {
    pub total: u64,
    pub success: u64,
    pub error: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresAgentSummary {
    pub agent_id: String,
    pub total_actions: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub denial_count: u64,
    pub success_rate: f64,
    pub last_active_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresUnmetIntentAggRow {
    pub entity_type: String,
    pub action: String,
    pub error: Option<String>,
    pub count: u64,
    pub first_seen: String,
    pub last_seen: String,
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
    pub agent_type: Option<&'a str>,
}

impl PostgresEventStore {
    pub async fn persist_trajectory(
        &self,
        entry: PostgresTrajectoryInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let created_at = parse_rfc3339(entry.created_at)?;
        let request_body = parse_optional_json(entry.request_body)?;
        let matched_policy_ids = parse_optional_json(entry.matched_policy_ids)?;
        crate::dbm::postgres_query!(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
              agent_id, session_id, authz_denied, denied_resource, denied_module, source, \
              spec_governed, created_at, request_body, intent, matched_policy_ids, agent_type) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
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
        .bind(entry.agent_type)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<PostgresTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                    agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                    created_at, request_body, intent, matched_policy_ids \
             FROM trajectories \
             ORDER BY created_at DESC \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_trajectory).collect())
    }

    pub async fn load_unmet_intent_rows(
        &self,
    ) -> Result<Vec<PostgresUnmetIntentAggRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT entity_type, MAX(action) AS action, error, COUNT(*)::bigint AS cnt, \
                    MIN(created_at) AS first_seen, MAX(created_at) AS last_seen \
             FROM trajectories \
             WHERE success = false AND (authz_denied IS NULL OR authz_denied = false) \
             GROUP BY entity_type, error \
             ORDER BY cnt DESC \
             LIMIT 100",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_unmet_intent).collect())
    }

    pub async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, MAX(created_at) AS latest_at \
             FROM trajectories \
             WHERE success = true AND action = 'SubmitSpec' \
             GROUP BY entity_type",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(entity_type, latest_at)| (entity_type, latest_at.to_rfc3339()))
            .collect())
    }

    pub async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        let rows: Vec<(String, i64)> = crate::dbm::postgres_query_as!(
            "SELECT tenant, COUNT(*)::bigint FROM trajectories GROUP BY tenant"
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(tenant, count)| (tenant, count as u64))
            .collect())
    }

    pub async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<PostgresTrajectoryStats, PersistenceError> {
        let row: (i64, i64) = crate::dbm::postgres_query_as!(
            "SELECT COUNT(*)::bigint AS total, \
                    COALESCE(SUM(CASE WHEN success = true THEN 1 ELSE 0 END), 0)::bigint AS success_count \
             FROM trajectories \
             WHERE ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::boolean IS NULL OR success = $3)",
        )
        .bind(entity_type)
        .bind(action)
        .bind(success_filter)
        .fetch_one(self.pool())
        .await
        .map_err(storage_error)?;
        let total = row.0 as u64;
        let success_count = row.1 as u64;

        let action_rows: Vec<(String, i64, i64, i64)> = crate::dbm::postgres_query_as!(
            "SELECT action, COUNT(*)::bigint AS total, \
                    COALESCE(SUM(CASE WHEN success = true THEN 1 ELSE 0 END), 0)::bigint AS success, \
                    COALESCE(SUM(CASE WHEN success = false THEN 1 ELSE 0 END), 0)::bigint AS error \
             FROM trajectories \
             GROUP BY action",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        let by_action = action_rows
            .into_iter()
            .map(|(name, total, success, error)| {
                (
                    name,
                    PostgresActionStats {
                        total: total as u64,
                        success: success as u64,
                        error: error as u64,
                    },
                )
            })
            .collect();

        let failed_rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                    agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                    created_at, request_body, intent, matched_policy_ids \
             FROM trajectories \
             WHERE success = false \
             ORDER BY created_at DESC \
             LIMIT $1",
        )
        .bind(failed_limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        let failed_intents = failed_rows.into_iter().map(row_to_trajectory).collect();
        let error_count = total.saturating_sub(success_count);
        Ok(PostgresTrajectoryStats {
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

    pub async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                    agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                    created_at, request_body, intent, matched_policy_ids \
             FROM trajectories \
             WHERE agent_id = $1 \
               AND ($2::text IS NULL OR tenant = $2) \
               AND ($3::text IS NULL OR entity_type = $3) \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(agent_id)
        .bind(tenant)
        .bind(entity_type)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_trajectory).collect())
    }

    pub async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<PostgresAgentSummary>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT agent_id, COUNT(*)::bigint AS total_actions, \
                    COALESCE(SUM(CASE WHEN success = true THEN 1 ELSE 0 END), 0)::bigint AS success_count, \
                    COALESCE(SUM(CASE WHEN success = false THEN 1 ELSE 0 END), 0)::bigint AS error_count, \
                    COALESCE(SUM(CASE WHEN authz_denied = true THEN 1 ELSE 0 END), 0)::bigint AS denial_count, \
                    MAX(created_at) AS last_active_at \
             FROM trajectories \
             WHERE agent_id IS NOT NULL AND ($1::text IS NULL OR tenant = $1) \
             GROUP BY agent_id \
             ORDER BY last_active_at DESC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_agent_summary).collect())
    }
}

fn row_to_trajectory(row: sqlx::postgres::PgRow) -> PostgresTrajectoryRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let request_body: Option<serde_json::Value> = row.get("request_body");
    let matched_policy_ids: Option<serde_json::Value> = row.get("matched_policy_ids");
    PostgresTrajectoryRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        action: row.get("action"),
        success: row.get("success"),
        from_status: row.get("from_status"),
        to_status: row.get("to_status"),
        error: row.get("error"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        authz_denied: row.get("authz_denied"),
        denied_resource: row.get("denied_resource"),
        denied_module: row.get("denied_module"),
        source: row.get("source"),
        spec_governed: row.get("spec_governed"),
        created_at: created_at.to_rfc3339(),
        request_body: request_body.map(|value| value.to_string()),
        intent: row.get("intent"),
        matched_policy_ids: matched_policy_ids
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok()),
    }
}

fn row_to_unmet_intent(row: sqlx::postgres::PgRow) -> PostgresUnmetIntentAggRow {
    let count: i64 = row.get("cnt");
    let first_seen: chrono::DateTime<chrono::Utc> = row.get("first_seen");
    let last_seen: chrono::DateTime<chrono::Utc> = row.get("last_seen");
    PostgresUnmetIntentAggRow {
        entity_type: row.get("entity_type"),
        action: row.get("action"),
        error: row.get("error"),
        count: count as u64,
        first_seen: first_seen.to_rfc3339(),
        last_seen: last_seen.to_rfc3339(),
    }
}

fn row_to_agent_summary(row: sqlx::postgres::PgRow) -> PostgresAgentSummary {
    let total = row.get::<i64, _>("total_actions") as u64;
    let success = row.get::<i64, _>("success_count") as u64;
    let last_active_at: chrono::DateTime<chrono::Utc> = row.get("last_active_at");
    PostgresAgentSummary {
        agent_id: row.get("agent_id"),
        total_actions: total,
        success_count: success,
        error_count: row.get::<i64, _>("error_count") as u64,
        denial_count: row.get::<i64, _>("denial_count") as u64,
        success_rate: if total > 0 {
            success as f64 / total as f64
        } else {
            0.0
        },
        last_active_at: last_active_at.to_rfc3339(),
    }
}
