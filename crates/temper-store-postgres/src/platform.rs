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

mod evolution;
mod inputs;
mod policy_approval;
mod rows;
pub use inputs::{PostgresEvolutionRecordInsert, PostgresPolicyApprovalCommit};
use rows::*;

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

pub(crate) type ScalarFieldIndex = BTreeMap<String, String>;
type CatalogProjectionFingerprint = (String, String, i64);

pub(crate) fn canonical_projection_status<'a>(
    fallback: &'a str,
    state: &'a serde_json::Value,
) -> &'a str {
    state
        .get("status")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
        .unwrap_or(fallback)
}

struct QueryProjectionCatalogUpdate<'a> {
    tenant: &'a str,
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    fields: &'a serde_json::Value,
    state: &'a serde_json::Value,
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
             state = $8, \
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
    .bind(update.state)
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
    pub source_kind: String,
    pub app_ref: String,
    pub version_hash: String,
    pub pinned_version_hash: String,
    pub current_version_hash: String,
    pub follow_policy: String,
    pub closure_id: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub dependency_lock_digest: String,
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
    pub state: Option<serde_json::Value>,
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
    /// Monotonic capture order stamped by the process that recorded the row.
    /// Null on rows written before the column existed.
    pub capture_seq: Option<i64>,
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
    pub tenant: String,
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
    pub tenant: String,
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
    pub persistence_status: String,
    pub persist_attempts: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A stored OTS trajectory document together with the run identity recorded
/// alongside it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresOtsTrajectoryDocument {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub data: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresQueuedOtsTrajectoryRow {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub data: String,
    pub persist_attempts: i64,
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
    /// Monotonic capture order stamped by the recording process, so a session
    /// reads back in the order the kernel captured it rather than the order
    /// independent persistence tasks happened to land.
    pub capture_seq: Option<i64>,
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
              spec_governed, created_at, request_body, intent, matched_policy_ids, capture_seq) \
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
        .bind(entry.capture_seq)
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
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": status,
            "item_count": 0,
            "counters": {},
            "booleans": {},
            "lists": {},
            "fields": fields,
            "events": [],
            "total_event_count": sequence_nr,
            "sequence_nr": sequence_nr
        });
        self.upsert_query_projection_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            sequence_nr,
        )
        .await
    }

    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    pub async fn upsert_query_projection_with_state(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let status = canonical_projection_status(status, state);
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
            record_postgres_projection_index_fields(
                QUERY_PROJECTION_UPSERT_OPERATION,
                entity_type,
                indexed_fields,
                skipped_fields,
            );
            record_postgres_projection_index_reconciliation(
                QUERY_PROJECTION_UPSERT_OPERATION,
                "stale_skipped",
            );
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
                    state,
                    sequence_nr,
                    projection_hash: projection_hash.as_str(),
                },
            )
            .await?;
            previous_catalog
        } else {
            let inserted: Option<i32> = crate::dbm::postgres_query_scalar!(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, state, sequence_nr, projection_version, projection_hash, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 2, $8, now()) \
                 ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING \
                 RETURNING 1",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(status)
            .bind(fields)
            .bind(state)
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
                    record_postgres_projection_index_fields(
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        entity_type,
                        indexed_fields,
                        skipped_fields,
                    );
                    record_postgres_projection_index_reconciliation(
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        "stale_skipped",
                    );
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
                        state,
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
        record_postgres_projection_index_fields(
            QUERY_PROJECTION_UPSERT_OPERATION,
            entity_type,
            indexed_fields,
            skipped_fields,
        );
        record_postgres_projection_index_reconciliation(
            QUERY_PROJECTION_UPSERT_OPERATION,
            reconciliation_path,
        );
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
        let rows: Vec<(
            String,
            String,
            serde_json::Value,
            Option<serde_json::Value>,
            i64,
        )> = crate::dbm::postgres_query_as!(
            "SELECT entity_id, status, fields, state, sequence_nr \
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
            .map(|(entity_id, status, fields, state, seq)| {
                crate::platform::PostgresEntityCatalogRow {
                    entity_id,
                    status,
                    fields,
                    state,
                    sequence_nr: seq.max(0) as u64,
                }
            })
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
            source_kind: "local".to_string(),
            app_ref: String::new(),
            version_hash: String::new(),
            pinned_version_hash: String::new(),
            current_version_hash: String::new(),
            follow_policy: "pinned".to_string(),
            closure_id: String::new(),
            registry_url: String::new(),
            registry_tenant: String::new(),
            dependency_lock_digest: String::new(),
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
             (tenant, app_name, source_kind, app_ref, version_hash, pinned_version_hash, current_version_hash, follow_policy, closure_id, registry_url, registry_tenant, dependency_lock_digest, \
              app_version, bundle_digest, spec_digest, policy_digest, wasm_digest, \
              content_digest, seed_digest, installed_at, last_reconciled_at, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, now(), $20, $21) \
             ON CONFLICT (tenant, app_name) DO UPDATE SET \
                 source_kind = EXCLUDED.source_kind, app_ref = EXCLUDED.app_ref, \
                 version_hash = EXCLUDED.version_hash, pinned_version_hash = EXCLUDED.pinned_version_hash, \
                 current_version_hash = EXCLUDED.current_version_hash, follow_policy = EXCLUDED.follow_policy, \
                 closure_id = EXCLUDED.closure_id, \
                 registry_url = EXCLUDED.registry_url, registry_tenant = EXCLUDED.registry_tenant, \
                 dependency_lock_digest = EXCLUDED.dependency_lock_digest, \
                 app_version = EXCLUDED.app_version, bundle_digest = EXCLUDED.bundle_digest, \
                 spec_digest = EXCLUDED.spec_digest, policy_digest = EXCLUDED.policy_digest, \
                 wasm_digest = EXCLUDED.wasm_digest, content_digest = EXCLUDED.content_digest, \
                 seed_digest = EXCLUDED.seed_digest, last_reconciled_at = EXCLUDED.last_reconciled_at, status = EXCLUDED.status",
        )
        .bind(&record.tenant)
        .bind(&record.app_name)
        .bind(&record.source_kind)
        .bind(&record.app_ref)
        .bind(&record.version_hash)
        .bind(&record.pinned_version_hash)
        .bind(&record.current_version_hash)
        .bind(&record.follow_policy)
        .bind(&record.closure_id)
        .bind(&record.registry_url)
        .bind(&record.registry_tenant)
        .bind(&record.dependency_lock_digest)
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
            "SELECT tenant, app_name, source_kind, app_ref, version_hash, pinned_version_hash, current_version_hash, follow_policy, closure_id, registry_url, registry_tenant, dependency_lock_digest, \
                    app_version, bundle_digest, spec_digest, policy_digest, \
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
        let result = crate::dbm::postgres_query!(
            "INSERT INTO pending_decisions (id, tenant, status, data, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, \
                 data = EXCLUDED.data, updated_at = now() \
             WHERE pending_decisions.tenant = EXCLUDED.tenant",
        )
        .bind(id)
        .bind(tenant)
        .bind(status)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        if result.rows_affected() != 1 {
            return Err(PersistenceError::Storage(format!(
                "pending decision '{id}' is owned by another tenant"
            )));
        }
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

    /// Load approved decisions whose scope names `session_id`, for one tenant.
    ///
    /// Backs session-grant validation (ADR-0157): a caller-asserted session id
    /// becomes a Cedar input only when an approved decision binds that session,
    /// so the lookup filters on the approved scope's session, not the denial's.
    pub async fn load_approved_session_decisions(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM pending_decisions \
             WHERE tenant = $1 \
               AND status = 'approved' \
               AND data->'approved_scope'->>'session_id' = $2",
        )
        .bind(tenant)
        .bind(session_id)
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
        tenant: &str,
        limit: i64,
    ) -> Result<Vec<PostgresTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                    agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                    created_at, request_body, intent, matched_policy_ids, capture_seq \
             FROM trajectories \
             WHERE tenant = $1 \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_trajectory).collect())
    }

    pub async fn load_unmet_intent_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresUnmetIntentAggRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT entity_type, MAX(action) AS action, error, COUNT(*)::bigint AS cnt, \
                    MIN(created_at) AS first_seen, MAX(created_at) AS last_seen \
             FROM trajectories \
             WHERE tenant = $1 \
               AND success = false AND (authz_denied IS NULL OR authz_denied = false) \
             GROUP BY entity_type, error \
             ORDER BY cnt DESC \
             LIMIT 100",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_unmet_intent).collect())
    }

    pub async fn load_submit_spec_timestamps(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, MAX(created_at) AS latest_at \
             FROM trajectories \
             WHERE tenant = $1 AND success = true AND action = 'SubmitSpec' \
             GROUP BY entity_type",
        )
        .bind(tenant)
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

    /// Trajectory statistics for one tenant.
    ///
    /// Scoped to `tenant` in SQL: the failed-intent list returns whole rows —
    /// error strings, entity ids — so an unscoped read would hand one tenant
    /// another's operational detail (ADR-0157).
    pub async fn query_trajectory_stats(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<PostgresTrajectoryStats, PersistenceError> {
        let row: (i64, i64) = crate::dbm::postgres_query_as!(
            "SELECT COUNT(*)::bigint AS total, \
                    COALESCE(SUM(CASE WHEN success = true THEN 1 ELSE 0 END), 0)::bigint AS success_count \
             FROM trajectories \
             WHERE tenant = $1 \
               AND ($2::text IS NULL OR entity_type = $2) \
               AND ($3::text IS NULL OR action = $3) \
               AND ($4::boolean IS NULL OR success = $4)",
        )
        .bind(tenant)
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
             WHERE tenant = $1 \
             GROUP BY action",
        )
        .bind(tenant)
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
                    created_at, request_body, intent, matched_policy_ids, capture_seq \
             FROM trajectories \
             WHERE tenant = $1 \
               AND success = false \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(tenant)
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
                    created_at, request_body, intent, matched_policy_ids, capture_seq \
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

    /// Query one session's trajectory rows in the order the kernel wrote them.
    ///
    /// Ordered ascending — oldest first — because the conformance checker
    /// replays a session as a state-machine run and a newest-first read would
    /// hand it the run backwards.
    ///
    /// Ties inside one `created_at` tick are broken by `capture_seq`, the
    /// order the capturing process stamped on the entry, and only then by
    /// `id`. `id` alone is the order the writes landed: rows are persisted by
    /// independently spawned tasks, so two entries captured in one tick can be
    /// inserted in either order and a denial/retry pair would be replayed
    /// backwards. `COALESCE` sorts rows written before the column existed
    /// first and identically on both backends, rather than leaving it to each
    /// engine's NULL ordering.
    pub async fn query_trajectories_by_session(
        &self,
        session_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, \
                    agent_id, session_id, authz_denied, denied_resource, denied_module, source, spec_governed, \
                    created_at, request_body, intent, matched_policy_ids, capture_seq \
             FROM trajectories \
             WHERE session_id = $1 \
               AND ($2::text IS NULL OR tenant = $2) \
               AND ($3::text IS NULL OR entity_type = $3) \
             ORDER BY created_at ASC, COALESCE(capture_seq, 0) ASC, id ASC \
             LIMIT $4",
        )
        .bind(session_id)
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

    /// Persist a full OTS trajectory JSON blob.
    ///
    /// Identity is `(tenant, trajectory_id)`: the id comes from the uploading
    /// harness and one database holds every tenant's rows, so keying on the id
    /// alone would let one tenant's upload replace another's row — tenant
    /// column included.
    pub async fn persist_ots_trajectory(
        &self,
        p: &PostgresOtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(p.data)?;
        crate::dbm::postgres_query!(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, persistence_status, persist_attempts, last_error, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'persisted', 0, NULL, now(), now()) \
             ON CONFLICT (tenant, trajectory_id) DO UPDATE SET \
                 agent_id = EXCLUDED.agent_id, session_id = EXCLUDED.session_id, \
                 outcome = EXCLUDED.outcome, turn_count = EXCLUDED.turn_count, data = EXCLUDED.data, \
                 persistence_status = 'persisted', persist_attempts = 0, last_error = NULL, updated_at = now()",
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

    pub async fn enqueue_ots_trajectory(
        &self,
        p: &PostgresOtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        let data = parse_json(p.data)?;
        crate::dbm::postgres_query!(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, persistence_status, persist_attempts, last_error, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued', 0, NULL, now(), now()) \
             ON CONFLICT (tenant, trajectory_id) DO UPDATE SET \
                 agent_id = EXCLUDED.agent_id, session_id = EXCLUDED.session_id, \
                 outcome = EXCLUDED.outcome, turn_count = EXCLUDED.turn_count, data = EXCLUDED.data, \
                 persistence_status = 'queued', last_error = NULL, updated_at = now()",
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
        crate::dbm::postgres_query!(
            "UPDATE ots_trajectories \
             SET persistence_status = 'persisted', last_error = NULL, updated_at = now() \
             WHERE tenant = $1 AND trajectory_id = $2",
        )
        .bind(tenant)
        .bind(trajectory_id)
        .execute(self.pool())
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
        crate::dbm::postgres_query!(
            "UPDATE ots_trajectories \
             SET persistence_status = 'failed', persist_attempts = persist_attempts + 1, last_error = $3, updated_at = now() \
             WHERE tenant = $1 AND trajectory_id = $2",
        )
        .bind(tenant)
        .bind(trajectory_id)
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<PostgresQueuedOtsTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT trajectory_id, tenant, agent_id, COALESCE(session_id, '') AS session_id, outcome, turn_count, data, persist_attempts \
             FROM ots_trajectories \
             WHERE persistence_status = 'queued' \
             ORDER BY updated_at ASC, created_at ASC \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_queued_ots_trajectory).collect())
    }

    pub async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresOtsTrajectoryRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT trajectory_id, tenant, agent_id, COALESCE(session_id, '') AS session_id, outcome, turn_count, persistence_status, persist_attempts, last_error, created_at, updated_at \
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

    /// Load full OTS trajectory data by tenant and ID.
    ///
    /// The tenant is part of the lookup rather than a post-filter: one store
    /// holds every tenant's rows, so a caller that takes the trajectory id
    /// from a request path would otherwise read across tenants.
    pub async fn get_ots_trajectory(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<PostgresOtsTrajectoryDocument>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT agent_id, COALESCE(session_id, '') AS session_id, outcome, data \
             FROM ots_trajectories WHERE tenant = $1 AND trajectory_id = $2"
        )
        .bind(tenant)
        .bind(trajectory_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(|row| row_to_ots_document(row, tenant.to_string(), trajectory_id.to_string())))
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

    /// Retrieve a blob only when its durable size metadata is within the
    /// caller's allocation budget. `None` means missing or over budget.
    pub async fn get_blob_if_size_at_most(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        let max_bytes = i64::try_from(max_bytes)
            .map_err(|_| "legacy blob read budget exceeds i64".to_string())?;
        crate::dbm::postgres_query_scalar!(
            "SELECT data FROM blobs WHERE blob_key = $1 AND octet_length(data) <= $2"
        )
        .bind(key)
        .bind(max_bytes)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| format!("bounded blob get failed: {e}"))
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

    pub async fn get_pending_decision(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let row: Option<serde_json::Value> = crate::dbm::postgres_query_scalar!(
            "SELECT data FROM pending_decisions WHERE tenant = $1 AND id = $2"
        )
        .bind(tenant)
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

pub(crate) fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
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

pub(crate) fn json_hash(value: &serde_json::Value) -> String {
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

pub(crate) fn scalar_index_fields(fields: &serde_json::Value) -> (ScalarFieldIndex, u64, u64) {
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

pub(crate) fn postgres_placeholders(sql: &str, max_index: usize) -> String {
    let mut out = sql.to_string();
    for index in (1..=max_index).rev() {
        out = out.replace(&format!("?{index}"), &format!("${index}"));
    }
    out
}
