//! PostgreSQL platform-store methods.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use sqlx::{Acquire, Postgres, Row, Transaction};
use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;
use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_projection_index_fields, record_postgres_projection_index_reconciliation,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};

const DISTINCT_RESOURCE_IDS_BUDGET: usize = 100;
const BUNDLED_REPLACE_UPLOAD_SOURCE: &str = "bundled-replace-upload";

/// Maximum bytes for a single value to be indexed into `entity_field_index`.
/// Postgres btree (idx_efi_lookup) rejects keys that exceed roughly 2704 bytes
/// (one third of an 8KB page). Anything larger can't be indexed at all, so we
/// skip it from the per-field index — the full value remains in
/// `entity_catalog.fields` (jsonb, no size cap) for direct reads.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;
const QUERY_PROJECTION_UPSERT_OPERATION: &str = "query_projection_upsert";
const QUERY_PROJECTION_REMOVE_OPERATION: &str = "query_projection_remove";

type ScalarFieldIndex = BTreeMap<String, String>;
type CatalogProjectionFingerprint = (String, String, i64);

struct QueryProjectionCatalogUpdate<'a> {
    tenant: &'a str,
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    fields: &'a serde_json::Value,
    sequence_nr: u64,
    projection_hash: &'a str,
}

async fn update_query_projection_catalog_row(
    tx: &mut Transaction<'_, Postgres>,
    update: QueryProjectionCatalogUpdate<'_>,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        "UPDATE entity_catalog \
         SET status = $4, \
             fields = CASE WHEN projection_hash IS DISTINCT FROM $7 THEN $5 ELSE fields END, \
             sequence_nr = $6, \
             projection_version = 2, \
             projection_hash = $7, \
             updated_at = now() \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(update.tenant)
    .bind(update.entity_type)
    .bind(update.entity_id)
    .bind(update.status)
    .bind(update.fields)
    .bind(update.sequence_nr as i64)
    .bind(update.projection_hash)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn reconcile_query_projection_field_index(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    status: &str,
    new_index: &ScalarFieldIndex,
) -> Result<(), PersistenceError> {
    let mut field_names = Vec::with_capacity(new_index.len());
    let mut field_values = Vec::with_capacity(new_index.len());
    for (field_name, field_value) in new_index {
        field_names.push(field_name.clone());
        field_values.push(field_value.clone());
    }

    crate::dbm::postgres_query!(
        "DELETE FROM entity_field_index e \
         WHERE e.tenant = $1 \
           AND e.entity_type = $2 \
           AND e.entity_id = $3 \
           AND NOT EXISTS ( \
               SELECT 1 \
               FROM unnest($4::text[], $5::text[]) AS incoming(field_name, field_value) \
               WHERE incoming.field_name = e.field_name \
                 AND incoming.field_value IS NOT DISTINCT FROM e.field_value \
                 AND $6 IS NOT DISTINCT FROM e.status \
           )",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(&field_names)
    .bind(&field_values)
    .bind(status)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;

    if !field_names.is_empty() {
        crate::dbm::postgres_query!(
            "INSERT INTO entity_field_index \
             (tenant, entity_type, entity_id, field_name, field_value, status) \
             SELECT $1, $2, $3, incoming.field_name, incoming.field_value, $6 \
             FROM unnest($4::text[], $5::text[]) AS incoming(field_name, field_value) \
             ON CONFLICT (tenant, entity_type, entity_id, field_name) DO UPDATE SET \
                 field_value = EXCLUDED.field_value, \
                 status = EXCLUDED.status \
             WHERE entity_field_index.field_value IS DISTINCT FROM EXCLUDED.field_value \
                OR entity_field_index.status IS DISTINCT FROM EXCLUDED.status",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(&field_names)
        .bind(&field_values)
        .bind(status)
        .execute(&mut **tx)
        .await
        .map_err(storage_error)?;
    }

    Ok(())
}

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
    /// Provenance: `"bundled"` (install pipeline) or `"upload"` (hot upload).
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct PostgresWasmModuleMetadataRow {
    pub tenant: String,
    pub module_name: String,
    pub sha256_hash: String,
    pub size_bytes: i32,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresWasmInvocationInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub module_name: &'a str,
    pub trigger_action: &'a str,
    pub callback_action: Option<&'a str>,
    pub success: bool,
    pub error: Option<&'a str>,
    pub duration_ms: u64,
    pub created_at: &'a str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresWasmInvocationRow {
    pub tenant: String,
    pub entity_type: String,
    pub entity_id: String,
    pub module_name: String,
    pub trigger_action: String,
    pub callback_action: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub created_at: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresProjectedEntityFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// One row from `entity_catalog`, with the full JSONB fields blob preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct PostgresEntityCatalogRow {
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub sequence_nr: u64,
}

pub type PostgresSecretRow = (String, Vec<u8>, Vec<u8>);

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresFeatureRequestRow {
    pub id: String,
    pub category: String,
    pub description: String,
    pub frequency: i64,
    pub trajectory_refs: String,
    pub disposition: String,
    pub developer_notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresEvolutionRecordRow {
    pub id: String,
    pub record_type: String,
    pub status: String,
    pub created_by: String,
    pub derived_from: Option<String>,
    pub data: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresDesignTimeEventRow {
    pub id: i64,
    pub kind: String,
    pub entity_type: String,
    pub tenant: String,
    pub summary: String,
    pub level: Option<String>,
    pub passed: Option<bool>,
    pub step_number: Option<i64>,
    pub total_steps: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresOtsTrajectoryRow {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresOtsTrajectoryParams<'a> {
    pub trajectory_id: &'a str,
    pub tenant: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub outcome: &'a str,
    pub turn_count: i64,
    pub data: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PostgresPublishedArtifactRow {
    pub id: String,
    pub tenant: String,
    pub source_file_id: String,
    pub source_file_version_id: String,
    pub content_hash: String,
    pub label: String,
    pub mime_type: String,
    pub byte_length: i64,
    pub public_storage_key: String,
    pub public_url: String,
    pub owner_ref_type: String,
    pub owner_ref_id: String,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostgresPublishedArtifactUpsert<'a> {
    pub id: &'a str,
    pub tenant: &'a str,
    pub source_file_id: &'a str,
    pub source_file_version_id: &'a str,
    pub content_hash: &'a str,
    pub label: &'a str,
    pub mime_type: &'a str,
    pub byte_length: i64,
    pub public_storage_key: &'a str,
    pub public_url: &'a str,
    pub owner_ref_type: &'a str,
    pub owner_ref_id: &'a str,
    pub status: &'a str,
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
        crate::dbm::postgres_query!(
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
        let (new_index, indexed_fields, skipped_fields) = scalar_index_fields(fields);
        let mut transaction_timer =
            PostgresTransactionTimer::start(QUERY_PROJECTION_UPSERT_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool().acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };

        let previous_catalog: Option<CatalogProjectionFingerprint> =
            crate::dbm::postgres_query_as!(
                "SELECT status, projection_hash, sequence_nr \
                 FROM entity_catalog \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 FOR UPDATE",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;

        let new_sequence_nr = sequence_nr as i64;
        let previous_catalog = if previous_catalog
            .as_ref()
            .is_some_and(|(_, _, existing_sequence)| *existing_sequence > new_sequence_nr)
        {
            let commit_started = Instant::now();
            tx.commit().await.map_err(|e| {
                record_postgres_transaction_commit_duration(
                    commit_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                storage_error(e)
            })?;
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_UPSERT_OPERATION,
                "ok",
            );
            record_postgres_projection_index_fields(indexed_fields, skipped_fields);
            record_postgres_projection_index_reconciliation("stale_skipped");
            transaction_timer.set_outcome("stale_skipped");
            return Ok(());
        } else if previous_catalog.is_some() {
            update_query_projection_catalog_row(
                &mut tx,
                QueryProjectionCatalogUpdate {
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    fields,
                    sequence_nr,
                    projection_hash: projection_hash.as_str(),
                },
            )
            .await?;
            previous_catalog
        } else {
            let inserted: Option<i32> = crate::dbm::postgres_query_scalar!(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, sequence_nr, projection_version, projection_hash, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, 2, $7, now()) \
                 ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING \
                 RETURNING 1",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(status)
            .bind(fields)
            .bind(sequence_nr as i64)
            .bind(projection_hash.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;

            if inserted.is_some() {
                None
            } else {
                let raced_catalog: Option<CatalogProjectionFingerprint> =
                    crate::dbm::postgres_query_as!(
                        "SELECT status, projection_hash, sequence_nr \
                         FROM entity_catalog \
                         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                         FOR UPDATE",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(entity_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                if raced_catalog
                    .as_ref()
                    .is_some_and(|(_, _, existing_sequence)| *existing_sequence > new_sequence_nr)
                {
                    let commit_started = Instant::now();
                    tx.commit().await.map_err(|e| {
                        record_postgres_transaction_commit_duration(
                            commit_started.elapsed(),
                            QUERY_PROJECTION_UPSERT_OPERATION,
                            "error",
                        );
                        storage_error(e)
                    })?;
                    record_postgres_transaction_commit_duration(
                        commit_started.elapsed(),
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        "ok",
                    );
                    record_postgres_projection_index_fields(indexed_fields, skipped_fields);
                    record_postgres_projection_index_reconciliation("stale_skipped");
                    transaction_timer.set_outcome("stale_skipped");
                    return Ok(());
                }
                update_query_projection_catalog_row(
                    &mut tx,
                    QueryProjectionCatalogUpdate {
                        tenant,
                        entity_type,
                        entity_id,
                        status,
                        fields,
                        sequence_nr,
                        projection_hash: projection_hash.as_str(),
                    },
                )
                .await?;
                raced_catalog
            }
        };

        let should_reconcile_index =
            previous_catalog
                .as_ref()
                .is_none_or(|(old_status, old_hash, _)| {
                    old_status.as_str() != status || old_hash.as_str() != projection_hash.as_str()
                });
        let reconciliation_path = if should_reconcile_index {
            if previous_catalog.is_some() {
                "diff"
            } else {
                "insert"
            }
        } else {
            "skipped_unchanged"
        };

        if should_reconcile_index {
            reconcile_query_projection_field_index(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                status,
                &new_index,
            )
            .await?;
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_UPSERT_OPERATION,
                "error",
            );
            storage_error(e)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            QUERY_PROJECTION_UPSERT_OPERATION,
            "ok",
        );
        record_postgres_projection_index_fields(indexed_fields, skipped_fields);
        record_postgres_projection_index_reconciliation(reconciliation_path);
        transaction_timer.set_outcome("ok");
        Ok(())
    }

    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction_timer =
            PostgresTransactionTimer::start(QUERY_PROJECTION_REMOVE_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool().acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };
        crate::dbm::postgres_query!(
            "DELETE FROM entity_catalog WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3")
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_REMOVE_OPERATION,
                "error",
            );
            storage_error(e)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            QUERY_PROJECTION_REMOVE_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");
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
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query_scalar::<_, String>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        query.fetch_all(self.pool()).await.map_err(storage_error)
    }

    pub async fn load_query_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Vec<PostgresProjectedEntityFieldsRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let requested_fields: BTreeSet<String> = field_names
            .iter()
            .map(|field| (*field).to_string())
            .collect();
        let field_names = requested_fields.iter().cloned().collect::<Vec<_>>();
        let rows = crate::dbm::postgres_query!(
            "SELECT c.entity_id, c.status, f.field_name, f.field_value \
             FROM entity_catalog c \
             LEFT JOIN entity_field_index f \
               ON c.tenant = f.tenant \
              AND c.entity_type = f.entity_type \
              AND c.entity_id = f.entity_id \
              AND f.field_name = ANY($4) \
             WHERE c.tenant = $1 \
               AND c.entity_type = $2 \
               AND c.entity_id = ANY($3) \
             ORDER BY c.entity_id, f.field_name",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .bind(&field_names)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;

        let mut by_entity = BTreeMap::<String, PostgresProjectedEntityFieldsRow>::new();
        for row in rows {
            let entity_id: String = row.get("entity_id");
            let status: String = row.get("status");
            let field_name: Option<String> = row.get("field_name");
            let field_value: Option<String> = row.get("field_value");
            let entry = by_entity.entry(entity_id.clone()).or_insert_with(|| {
                PostgresProjectedEntityFieldsRow {
                    entity_id: entity_id.clone(),
                    status,
                    fields: requested_fields
                        .iter()
                        .map(|field| (field.clone(), None))
                        .collect(),
                }
            });
            if let Some(field_name) = field_name
                && requested_fields.contains(&field_name)
            {
                entry.fields.insert(field_name, field_value);
            }
        }

        Ok(entity_ids
            .iter()
            .filter_map(|entity_id| by_entity.remove(entity_id))
            .collect())
    }

    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Vec<(String, u64)>, PersistenceError> {
        let rows: Vec<(String, i64)> = crate::dbm::postgres_query_as!(
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

    /// Batch-load full entity catalog rows for a list of entity IDs.
    ///
    /// Returns only rows that exist in the catalog. IDs without a row in the
    /// projection are silently omitted from the result, leaving the caller
    /// free to fall back to the actor path on a per-id basis.
    pub async fn load_entity_catalog_rows_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<crate::platform::PostgresEntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, String, serde_json::Value, i64)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id, status, fields, sequence_nr \
             FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = ANY($3) \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(
                |(entity_id, status, fields, seq)| crate::platform::PostgresEntityCatalogRow {
                    entity_id,
                    status,
                    fields,
                    sequence_nr: seq.max(0) as u64,
                },
            )
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

    pub async fn load_all_wasm_modules(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresWasmModuleRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
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
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
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
        source: &str,
    ) -> Result<(), PersistenceError> {
        // Idempotent on hash + source-aware preservation:
        //   - source='upload' callers (hot upload via the API) overwrite anything
        //     so iterative testing works.
        //   - source='bundled' callers (the os-apps install pipeline) only
        //     overwrite existing 'bundled' rows. They preserve hot uploads
        //     across same-bundle restarts.
        //   - source='bundled-replace-upload' is an internal reconcile mode:
        //     persist the row back as 'bundled' while replacing stale uploads
        //     after the installed app's bundled WASM digest changed.
        let replace_uploaded_wasm = source == BUNDLED_REPLACE_UPLOAD_SOURCE;
        let persisted_source = if replace_uploaded_wasm {
            "bundled"
        } else {
            source
        };
        crate::dbm::postgres_query!(
            "INSERT INTO wasm_modules \
             (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source) \
             VALUES ($1, $2, $3, $4, 1, $5, now(), $6) \
             ON CONFLICT (tenant, module_name) DO UPDATE SET \
                 wasm_bytes = EXCLUDED.wasm_bytes, \
                 sha256_hash = EXCLUDED.sha256_hash, \
                 version = wasm_modules.version + 1, \
                 size_bytes = EXCLUDED.size_bytes, \
                 updated_at = now(), \
                 source = EXCLUDED.source \
             WHERE wasm_modules.sha256_hash IS DISTINCT FROM EXCLUDED.sha256_hash \
                AND ($7 OR EXCLUDED.source = 'upload' OR wasm_modules.source = 'bundled')",
        )
        .bind(tenant)
        .bind(name)
        .bind(bytes)
        .bind(hash)
        .bind(bytes.len() as i32)
        .bind(persisted_source)
        .bind(replace_uploaded_wasm)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }
}

impl PostgresEventStore {
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

    #[allow(clippy::too_many_arguments)]
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
        let trajectory_refs = parse_json(trajectory_refs_json)?;
        crate::dbm::postgres_query!(
            "INSERT INTO feature_requests \
             (id, category, description, frequency, trajectory_refs, disposition, developer_notes, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
             ON CONFLICT (id) DO UPDATE SET \
                 category = EXCLUDED.category, description = EXCLUDED.description, frequency = EXCLUDED.frequency, \
                 trajectory_refs = EXCLUDED.trajectory_refs, disposition = EXCLUDED.disposition, \
                 developer_notes = EXCLUDED.developer_notes, updated_at = now()",
        )
        .bind(id)
        .bind(category)
        .bind(description)
        .bind(frequency)
        .bind(trajectory_refs)
        .bind(disposition)
        .bind(developer_notes)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<PostgresFeatureRequestRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at \
             FROM feature_requests \
             WHERE ($1::text IS NULL OR disposition = $1) \
             ORDER BY frequency DESC, created_at DESC",
        )
        .bind(disposition)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_feature_request).collect())
    }

    pub async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "UPDATE feature_requests SET disposition = $2, developer_notes = $3, updated_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(disposition)
        .bind(developer_notes)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        let payload = parse_json(data_json)?;
        crate::dbm::postgres_query!(
            "INSERT INTO evolution_records (id, record_type, status, created_by, derived_from, payload, timestamp) \
             VALUES ($1, $2, $3, $4, $5, $6, now())",
        )
        .bind(id)
        .bind(record_type)
        .bind(status)
        .bind(created_by)
        .bind(derived_from)
        .bind(payload)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<PostgresEvolutionRecordRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT id, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_evolution_record))
    }

    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records \
             WHERE ($1::text IS NULL OR record_type = $1) \
               AND ($2::text IS NULL OR status = $2) \
             ORDER BY timestamp DESC",
        )
        .bind(record_type)
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_evolution_record).collect())
    }

    pub async fn list_ranked_insights(
        &self,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let mut rows = self.list_evolution_records(Some("Insight"), None).await?;
        rows.sort_by(|a, b| {
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
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
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
        crate::dbm::postgres_query!(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(kind)
        .bind(entity_type)
        .bind(tenant)
        .bind(summary)
        .bind(level)
        .bind(passed)
        .bind(step_number.map(|value| value as i16))
        .bind(total_steps.map(|value| value as i16))
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresDesignTimeEventRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at \
             FROM design_time_events \
             WHERE ($1::text IS NULL OR tenant = $1) \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_design_time_event).collect())
    }

    pub async fn persist_ots_trajectory(
        &self,
        p: &PostgresOtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(p.data)?;
        crate::dbm::postgres_query!(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
             ON CONFLICT (trajectory_id) DO UPDATE SET \
                 tenant = EXCLUDED.tenant, agent_id = EXCLUDED.agent_id, session_id = EXCLUDED.session_id, \
                 outcome = EXCLUDED.outcome, turn_count = EXCLUDED.turn_count, data = EXCLUDED.data, created_at = now()",
        )
        .bind(p.trajectory_id)
        .bind(p.tenant)
        .bind(p.agent_id)
        .bind(p.session_id)
        .bind(p.outcome)
        .bind(p.turn_count)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresOtsTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT trajectory_id, tenant, agent_id, COALESCE(session_id, '') AS session_id, outcome, turn_count, created_at \
             FROM ots_trajectories \
             WHERE tenant = $1 \
               AND ($2::text IS NULL OR agent_id = $2) \
               AND ($3::text IS NULL OR outcome = $3) \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(tenant)
        .bind(agent_id)
        .bind(outcome)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_ots_trajectory).collect())
    }

    pub async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let row: Option<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM ots_trajectories WHERE trajectory_id = $1"
        )
        .bind(trajectory_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(|value| value.to_string()))
    }

    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, None).await
    }

    pub async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let ttl_seconds = ttl.map(|duration| duration.as_secs() as i64);
        crate::dbm::postgres_query!(
            "INSERT INTO blobs (blob_key, data, size_bytes, expires_at) \
             VALUES ($1, $2, $3, CASE WHEN $4::bigint IS NULL THEN NULL ELSE now() + ($4::bigint * interval '1 second') END) \
             ON CONFLICT (blob_key) DO NOTHING",
        )
        .bind(key)
        .bind(data)
        .bind(data.len() as i64)
        .bind(ttl_seconds)
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| format!("blob put failed: {e}"))
    }

    pub async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        let result = crate::dbm::postgres_query!(
            "WITH doomed AS ( \
                 SELECT blob_key FROM blobs \
                 WHERE expires_at IS NOT NULL AND expires_at < now() \
                 LIMIT $1 \
             ) \
             DELETE FROM blobs USING doomed WHERE blobs.blob_key = doomed.blob_key",
        )
        .bind(max_rows as i64)
        .execute(self.pool())
        .await
        .map_err(|e| format!("blob sweep failed: {e}"))?;
        Ok(result.rows_affected())
    }

    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        crate::dbm::postgres_query_scalar!("SELECT data FROM blobs WHERE blob_key = $1")
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| format!("blob get failed: {e}"))
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "postgres.upsert_published_artifact",
        tenant = %artifact.tenant,
        artifact_label = %artifact.label,
        owner_ref_type = %artifact.owner_ref_type,
        owner_ref_id = %artifact.owner_ref_id,
    ))]
    pub async fn upsert_published_artifact(
        &self,
        artifact: &PostgresPublishedArtifactUpsert<'_>,
    ) -> Result<PostgresPublishedArtifactRow, PersistenceError> {
        crate::dbm::postgres_query!(
            "INSERT INTO published_artifacts (
                id, tenant, source_file_id, source_file_version_id, content_hash,
                label, mime_type, byte_length, public_storage_key, public_url,
                owner_ref_type, owner_ref_id, status, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now())
            ON CONFLICT(id) DO UPDATE SET
                tenant = EXCLUDED.tenant,
                source_file_id = EXCLUDED.source_file_id,
                source_file_version_id = EXCLUDED.source_file_version_id,
                content_hash = EXCLUDED.content_hash,
                label = EXCLUDED.label,
                mime_type = EXCLUDED.mime_type,
                byte_length = EXCLUDED.byte_length,
                public_storage_key = EXCLUDED.public_storage_key,
                public_url = EXCLUDED.public_url,
                owner_ref_type = EXCLUDED.owner_ref_type,
                owner_ref_id = EXCLUDED.owner_ref_id,
                status = EXCLUDED.status,
                updated_at = now()",
        )
        .bind(artifact.id)
        .bind(artifact.tenant)
        .bind(artifact.source_file_id)
        .bind(artifact.source_file_version_id)
        .bind(artifact.content_hash)
        .bind(artifact.label)
        .bind(artifact.mime_type)
        .bind(artifact.byte_length)
        .bind(artifact.public_storage_key)
        .bind(artifact.public_url)
        .bind(artifact.owner_ref_type)
        .bind(artifact.owner_ref_id)
        .bind(artifact.status)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;

        self.load_published_artifact(artifact.tenant, artifact.id)
            .await?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "published artifact '{}' was not readable after upsert",
                    artifact.id
                ))
            })
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "postgres.load_published_artifact",
        tenant,
        artifact_id,
    ))]
    pub async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PostgresPublishedArtifactRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT id, tenant, source_file_id, source_file_version_id, content_hash,
                    label, mime_type, byte_length, public_storage_key, public_url,
                    owner_ref_type, owner_ref_id, status
               FROM published_artifacts
              WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(artifact_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_published_artifact))
    }

    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (tenant, key_name) DO UPDATE SET ciphertext = EXCLUDED.ciphertext, nonce = EXCLUDED.nonce, updated_at = now()",
        )
        .bind(tenant)
        .bind(key_name)
        .bind(ciphertext)
        .bind(nonce)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn delete_secret(
        &self,
        tenant: &str,
        key_name: &str,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "DELETE FROM tenant_secrets WHERE tenant = $1 AND key_name = $2"
        )
        .bind(tenant)
        .bind(key_name)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn load_secrets_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresSecretRow>, PersistenceError> {
        crate::dbm::postgres_query_as!(
            "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1"
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
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

    pub async fn load_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<Option<PostgresWasmModuleRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
             FROM wasm_modules WHERE tenant = $1 AND module_name = $2",
        )
        .bind(tenant)
        .bind(module_name)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_wasm_module))
    }

    pub async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<PostgresWasmModuleMetadataRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, sha256_hash, size_bytes, updated_at \
             FROM wasm_modules ORDER BY tenant, module_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module_metadata).collect())
    }

    pub async fn persist_wasm_invocation(
        &self,
        entry: &PostgresWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let created_at = parse_rfc3339(entry.created_at)?;
        crate::dbm::postgres_query!(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(entry.tenant)
        .bind(entry.entity_type)
        .bind(entry.entity_id)
        .bind(entry.module_name)
        .bind(entry.trigger_action)
        .bind(entry.callback_action)
        .bind(entry.success)
        .bind(entry.error)
        .bind(entry.duration_ms as i64)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<PostgresWasmInvocationRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at \
             FROM wasm_invocation_logs \
             ORDER BY created_at DESC \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_invocation).collect())
    }

    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2"
        )
        .bind(tenant)
        .bind(module_name)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
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

fn scalar_index_fields(fields: &serde_json::Value) -> (ScalarFieldIndex, u64, u64) {
    let mut indexed = ScalarFieldIndex::new();
    let mut skipped_fields = 0_u64;

    if let Some(object) = fields.as_object() {
        for (field_name, value) in object {
            let Some(field_value) = scalar_field_value(value) else {
                continue;
            };
            // Postgres btree caps an indexed key at roughly one third of an
            // 8KB page. Long fields remain fully preserved in entity_catalog.
            if field_value.len() > MAX_INDEXABLE_FIELD_VALUE_BYTES {
                skipped_fields += 1;
                continue;
            }
            indexed.insert(field_name.clone(), field_value);
        }
    }

    let indexed_fields = indexed.len() as u64;
    (indexed, indexed_fields, skipped_fields)
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
    let source: Option<String> = row.try_get("source").ok();
    PostgresWasmModuleRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        wasm_bytes: row.get("wasm_bytes"),
        sha256_hash: row.get("sha256_hash"),
        source: source.unwrap_or_else(|| "bundled".to_string()),
    }
}

fn row_to_wasm_module_metadata(row: sqlx::postgres::PgRow) -> PostgresWasmModuleMetadataRow {
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresWasmModuleMetadataRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        sha256_hash: row.get("sha256_hash"),
        size_bytes: row.get("size_bytes"),
        updated_at: updated_at.to_rfc3339(),
    }
}

fn row_to_wasm_invocation(row: sqlx::postgres::PgRow) -> PostgresWasmInvocationRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let duration_ms: i64 = row.get("duration_ms");
    PostgresWasmInvocationRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        module_name: row.get("module_name"),
        trigger_action: row.get("trigger_action"),
        callback_action: row.get("callback_action"),
        success: row.get("success"),
        error: row.get("error"),
        duration_ms: duration_ms as u64,
        created_at: created_at.to_rfc3339(),
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

fn row_to_feature_request(row: sqlx::postgres::PgRow) -> PostgresFeatureRequestRow {
    let trajectory_refs: serde_json::Value = row.get("trajectory_refs");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresFeatureRequestRow {
        id: row.get("id"),
        category: row.get("category"),
        description: row.get("description"),
        frequency: row.get("frequency"),
        trajectory_refs: trajectory_refs.to_string(),
        disposition: row.get("disposition"),
        developer_notes: row.get("developer_notes"),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    }
}

fn row_to_evolution_record(row: sqlx::postgres::PgRow) -> PostgresEvolutionRecordRow {
    let payload: serde_json::Value = row.get("payload");
    let timestamp: chrono::DateTime<chrono::Utc> = row.get("timestamp");
    PostgresEvolutionRecordRow {
        id: row.get("id"),
        record_type: row.get("record_type"),
        status: row.get("status"),
        created_by: row.get("created_by"),
        derived_from: row.get("derived_from"),
        data: payload.to_string(),
        timestamp: timestamp.to_rfc3339(),
    }
}

fn row_to_design_time_event(row: sqlx::postgres::PgRow) -> PostgresDesignTimeEventRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let step_number: Option<i16> = row.get("step_number");
    let total_steps: Option<i16> = row.get("total_steps");
    PostgresDesignTimeEventRow {
        id: row.get("id"),
        kind: row.get("kind"),
        entity_type: row.get("entity_type"),
        tenant: row.get("tenant"),
        summary: row.get("summary"),
        level: row.get("level"),
        passed: row.get("passed"),
        step_number: step_number.map(i64::from),
        total_steps: total_steps.map(i64::from),
        created_at: created_at.to_rfc3339(),
    }
}

fn row_to_ots_trajectory(row: sqlx::postgres::PgRow) -> PostgresOtsTrajectoryRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    PostgresOtsTrajectoryRow {
        trajectory_id: row.get("trajectory_id"),
        tenant: row.get("tenant"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        outcome: row.get("outcome"),
        turn_count: row.get("turn_count"),
        created_at: created_at.to_rfc3339(),
    }
}

fn row_to_published_artifact(row: sqlx::postgres::PgRow) -> PostgresPublishedArtifactRow {
    PostgresPublishedArtifactRow {
        id: row.get("id"),
        tenant: row.get("tenant"),
        source_file_id: row.get("source_file_id"),
        source_file_version_id: row.get("source_file_version_id"),
        content_hash: row.get("content_hash"),
        label: row.get("label"),
        mime_type: row.get("mime_type"),
        byte_length: row.get("byte_length"),
        public_storage_key: row.get("public_storage_key"),
        public_url: row.get("public_url"),
        owner_ref_type: row.get("owner_ref_type"),
        owner_ref_id: row.get("owner_ref_id"),
        status: row.get("status"),
    }
}
