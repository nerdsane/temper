//! Durable aggregation for denied Cedar authorization shapes.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::super::{PolicyDenialPatternRow, TursoEventStore};
use crate::metrics::TursoQueryTimer;

const DISTINCT_RESOURCE_IDS_BUDGET: usize = 100;

impl TursoEventStore {
    /// Upsert a durable denial-pattern row for policy suggestion reconstruction.
    #[instrument(skip_all, fields(tenant, action, resource_type, otel.name = "turso.upsert_policy_denial_pattern"))]
    pub async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_policy_denial_pattern");
        let conn = self.configured_connection().await?;
        let agent_type_key = agent_type.unwrap_or("");

        let existing = {
            let mut rows = conn
                .query(
                    "SELECT count, first_seen, last_seen, distinct_resource_ids_json \
                     FROM policy_denial_patterns \
                     WHERE tenant = ?1 AND agent_type = ?2 AND action = ?3 AND resource_type = ?4",
                    params![tenant, agent_type_key, action, resource_type],
                )
                .await
                .map_err(storage_error)?;
            match rows.next().await.map_err(storage_error)? {
                Some(row) => Some((
                    row.get::<i64>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                    row.get::<String>(2).map_err(storage_error)?,
                    row.get::<String>(3).map_err(storage_error)?,
                )),
                None => None,
            }
        };

        let mut count = 1_i64;
        let mut first_seen = timestamp.to_string();
        let mut last_seen = timestamp.to_string();
        let mut distinct_resource_ids = std::collections::BTreeSet::new();

        if let Some((existing_count, existing_first_seen, existing_last_seen, ids_json)) = existing
        {
            count = existing_count + 1;
            first_seen = existing_first_seen;
            last_seen = if existing_last_seen.as_str() > timestamp {
                existing_last_seen
            } else {
                timestamp.to_string()
            };
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&ids_json) {
                distinct_resource_ids.extend(ids);
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

        let ids_json =
            serde_json::to_string(&distinct_resource_ids.into_iter().collect::<Vec<String>>())
                .map_err(storage_error)?;

        conn.execute(
            "INSERT INTO policy_denial_patterns \
             (tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(tenant, agent_type, action, resource_type) DO UPDATE SET \
                 count = excluded.count, \
                 first_seen = excluded.first_seen, \
                 last_seen = excluded.last_seen, \
                 distinct_resource_ids_json = excluded.distinct_resource_ids_json",
            params![
                tenant,
                agent_type_key,
                action,
                resource_type,
                count,
                first_seen,
                last_seen,
                ids_json,
            ],
        )
        .await
        .map_err(storage_error)?;

        Ok(())
    }

    /// Load durable denial patterns for one tenant, newest first.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_policy_denial_patterns"))]
    pub async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_policy_denial_patterns");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json \
                 FROM policy_denial_patterns \
                 WHERE tenant = ?1 \
                 ORDER BY last_seen DESC, count DESC",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let agent_type_raw = row.get::<String>(1).map_err(storage_error)?;
            out.push(PolicyDenialPatternRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                agent_type: if agent_type_raw.is_empty() {
                    None
                } else {
                    Some(agent_type_raw)
                },
                action: row.get::<String>(2).map_err(storage_error)?,
                resource_type: row.get::<String>(3).map_err(storage_error)?,
                count: row.get::<i64>(4).map_err(storage_error)?,
                first_seen: row.get::<String>(5).map_err(storage_error)?,
                last_seen: row.get::<String>(6).map_err(storage_error)?,
                distinct_resource_ids_json: row.get::<String>(7).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
