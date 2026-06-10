//! Query-projection catalog and per-field index maintenance.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use sqlx::{Acquire, Postgres, Row, Transaction};
use temper_runtime::persistence::PersistenceError;

use super::storage_error;
use crate::PostgresEventStore;
use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_projection_index_fields, record_postgres_projection_index_reconciliation,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};

/// Maximum bytes for a single value to be indexed into `entity_field_index`.
/// Postgres btree (idx_efi_lookup) rejects keys that exceed roughly 2704 bytes
/// (one third of an 8KB page). Anything larger can't be indexed at all, so we
/// skip it from the per-field index — the full value remains in
/// `entity_catalog.fields` (jsonb, no size cap) for direct reads.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;
const QUERY_PROJECTION_UPSERT_OPERATION: &str = "query_projection_upsert";
const QUERY_PROJECTION_REMOVE_OPERATION: &str = "query_projection_remove";

pub(crate) type ScalarFieldIndex = BTreeMap<String, String>;
type CatalogProjectionFingerprint = (String, String);

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

impl PostgresEventStore {
    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let projection_hash = super::json_hash(fields);
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
                "SELECT status, projection_hash \
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

        let previous_catalog = if previous_catalog.is_some() {
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
                        "SELECT status, projection_hash \
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
                .is_none_or(|(old_status, old_hash)| {
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

fn postgres_placeholders(sql: &str, max_index: usize) -> String {
    let mut out = sql.to_string();
    for index in (1..=max_index).rev() {
        out = out.replace(&format!("?{index}"), &format!("${index}"));
    }
    out
}
