//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::write_gate::WritePriority;
use super::{TursoEventStore, TursoQueryProjectionRow};

/// Match the Postgres query-plane limit: large scalar values stay in
/// `entity_catalog.fields` but are not copied into the filter index.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;

fn projection_hash(status: &str, fields: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(status.as_bytes());
    hasher.update(b"\n");

    if let Some(obj) = fields.as_object() {
        for (field_name, value) in obj {
            let field_value = scalar_to_text(value);
            if field_value.is_none() && !value.is_null() {
                continue;
            }
            hasher.update(field_name.as_bytes());
            hasher.update(b"=");
            match field_value {
                Some(field_value) => hasher.update(field_value.as_bytes()),
                None => hasher.update(b"<null>"),
            }
            hasher.update(b"\n");
        }
    }

    format!("{:x}", hasher.finalize())
}

/// Sparse projected field values loaded from the durable query plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedEntityFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// One row from `entity_catalog`, with the full JSON fields blob preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityCatalogRow {
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub sequence_nr: u64,
}

/// One durable projection upsert in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryProjectionUpsert {
    pub entity_type: String,
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub indexed_fields: serde_json::Value,
    pub sequence_nr: u64,
    pub known_new: bool,
}

impl TursoEventStore {
    /// Upsert the durable query-plane projection for a single entity.
    ///
    /// Maintains both:
    /// - `entity_catalog`: one live row per entity
    /// - `entity_field_index`: EAV rows for OData filter push-down
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_query_projection",
        tenant, entity_type, entity_id,
    ))]
    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.upsert_query_projection", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let new_projection_hash = projection_hash(status, fields);
        let fields_json = serde_json::to_string(fields)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let updated_at = sim_now().to_rfc3339();
        let sequence_nr = i64::try_from(sequence_nr).unwrap_or(i64::MAX);

        let unchanged_rows = conn
            .execute(
                "UPDATE entity_catalog \
                 SET status = ?4, updated_at = ?5, sequence_nr = ?6, projection_version = 2, fields = ?8 \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND projection_hash = ?7 \
                   AND sequence_nr <= ?6",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    updated_at.as_str(),
                    sequence_nr,
                    new_projection_hash.as_str(),
                    fields_json.as_str(),
                ],
            )
            .await
            .map_err(storage_error)?;

        if unchanged_rows > 0 {
            return Ok(());
        }

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let existing_sequence = {
            let mut existing_rows = tx
                .query(
                    "SELECT sequence_nr FROM entity_catalog \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            if let Some(row) = existing_rows.next().await.map_err(storage_error)? {
                Some(row.get::<i64>(0).map_err(storage_error)?)
            } else {
                None
            }
        };
        if existing_sequence.is_some_and(|existing| existing > sequence_nr) {
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }

        tx.execute(
            "INSERT INTO entity_catalog \
             (tenant, entity_type, entity_id, status, fields, updated_at, sequence_nr, projection_version, projection_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 2, ?8) \
             ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 status = excluded.status, \
                 fields = excluded.fields, \
                 updated_at = excluded.updated_at, \
                 sequence_nr = excluded.sequence_nr, \
                 projection_version = excluded.projection_version, \
                 projection_hash = excluded.projection_hash",
            params![
                tenant,
                entity_type,
                entity_id,
                status,
                fields_json.as_str(),
                updated_at.as_str(),
                sequence_nr,
                new_projection_hash.as_str(),
            ],
        )
        .await
        .map_err(storage_error)?;

        // Delete existing rows for this entity, then re-insert.
        // This is simpler than tracking individual field changes and handles
        // field removal correctly.
        tx.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;

        let indexed_fields = indexed_projection_fields(status, fields);
        if !indexed_fields.is_empty() {
            let mut sql = String::from(
                "INSERT INTO entity_field_index \
                 (tenant, entity_type, entity_id, field_name, field_value, status) VALUES ",
            );
            let mut values = Vec::with_capacity(indexed_fields.len() * 6);

            for (index, (field_name, field_value)) in indexed_fields.iter().enumerate() {
                if index > 0 {
                    sql.push_str(", ");
                }
                sql.push_str("(?, ?, ?, ?, ?, ?)");
                values.push(Value::from(tenant.to_string()));
                values.push(Value::from(entity_type.to_string()));
                values.push(Value::from(entity_id.to_string()));
                values.push(Value::from(field_name.clone()));
                values.push(
                    field_value
                        .as_ref()
                        .map(|value| Value::from(value.clone()))
                        .unwrap_or(Value::Null),
                );
                values.push(Value::from(status.to_string()));
            }

            tx.execute(&sql, params_from_iter(values))
                .await
                .map_err(storage_error)?;
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Upsert several durable query-plane projections in one transaction.
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_query_projections",
        tenant,
        projection_count = projections.len(),
    ))]
    pub async fn upsert_query_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        if projections.is_empty() {
            return Ok(());
        }

        let _write_permit = self
            .acquire_write_permit("turso.upsert_query_projections", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let updated_at = sim_now().to_rfc3339();
        let mut existing = BTreeMap::new();
        let existing_candidates = projections
            .iter()
            .filter(|projection| !projection.known_new)
            .collect::<Vec<_>>();
        if !existing_candidates.is_empty() {
            let mut existing_sql = String::from(
                "SELECT entity_type, entity_id, sequence_nr, projection_hash \
                 FROM entity_catalog WHERE tenant = ? AND (",
            );
            let mut existing_values = Vec::with_capacity(1 + existing_candidates.len() * 2);
            existing_values.push(Value::from(tenant.to_string()));
            for (index, projection) in existing_candidates.iter().enumerate() {
                if index > 0 {
                    existing_sql.push_str(" OR ");
                }
                existing_sql.push_str("(entity_type = ? AND entity_id = ?)");
                existing_values.push(Value::from(projection.entity_type.clone()));
                existing_values.push(Value::from(projection.entity_id.clone()));
            }
            existing_sql.push(')');

            let mut existing_rows = tx
                .query(&existing_sql, params_from_iter(existing_values))
                .await
                .map_err(storage_error)?;
            while let Some(row) = existing_rows.next().await.map_err(storage_error)? {
                let entity_type: String = row.get(0).map_err(storage_error)?;
                let entity_id: String = row.get(1).map_err(storage_error)?;
                let sequence_nr: i64 = row.get(2).map_err(storage_error)?;
                let projection_hash: String = row.get(3).map_err(storage_error)?;
                existing.insert((entity_type, entity_id), (sequence_nr, projection_hash));
            }
            drop(existing_rows);
        }

        struct PreparedProjection {
            entity_type: String,
            entity_id: String,
            status: String,
            fields_json: String,
            sequence_nr: i64,
            projection_hash: String,
        }

        let mut catalog_upserts = Vec::new();
        let mut changed_indexes = Vec::new();
        for projection in projections {
            let new_projection_hash = projection_hash(&projection.status, &projection.fields);
            let fields_json = serde_json::to_string(&projection.fields)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let sequence_nr = i64::try_from(projection.sequence_nr).unwrap_or(i64::MAX);
            let key = (projection.entity_type.clone(), projection.entity_id.clone());
            let existing_row = existing.get(&key);
            if existing_row.is_some_and(|(existing_sequence, _)| *existing_sequence > sequence_nr) {
                continue;
            }
            if !existing_row.is_some_and(|(_, existing_hash)| existing_hash == &new_projection_hash)
            {
                changed_indexes.push((
                    projection.entity_type.clone(),
                    projection.entity_id.clone(),
                    projection.status.clone(),
                    indexed_projection_fields(&projection.status, &projection.indexed_fields),
                    projection.known_new,
                ));
            }
            catalog_upserts.push(PreparedProjection {
                entity_type: projection.entity_type.clone(),
                entity_id: projection.entity_id.clone(),
                status: projection.status.clone(),
                fields_json,
                sequence_nr,
                projection_hash: new_projection_hash,
            });
        }

        if !catalog_upserts.is_empty() {
            let mut upsert_sql = String::from(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, updated_at, sequence_nr, projection_version, projection_hash) \
                 VALUES ",
            );
            let mut upsert_values = Vec::with_capacity(catalog_upserts.len() * 8);
            for (index, projection) in catalog_upserts.iter().enumerate() {
                if index > 0 {
                    upsert_sql.push_str(", ");
                }
                upsert_sql.push_str("(?, ?, ?, ?, ?, ?, ?, 2, ?)");
                upsert_values.push(Value::from(tenant.to_string()));
                upsert_values.push(Value::from(projection.entity_type.clone()));
                upsert_values.push(Value::from(projection.entity_id.clone()));
                upsert_values.push(Value::from(projection.status.clone()));
                upsert_values.push(Value::from(projection.fields_json.clone()));
                upsert_values.push(Value::from(updated_at.clone()));
                upsert_values.push(Value::from(projection.sequence_nr));
                upsert_values.push(Value::from(projection.projection_hash.clone()));
            }
            upsert_sql.push_str(
                " ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 status = excluded.status, \
                 fields = excluded.fields, \
                 updated_at = excluded.updated_at, \
                 sequence_nr = excluded.sequence_nr, \
                 projection_version = excluded.projection_version, \
                 projection_hash = excluded.projection_hash \
                 WHERE entity_catalog.sequence_nr <= excluded.sequence_nr",
            );
            tx.execute(&upsert_sql, params_from_iter(upsert_values))
                .await
                .map_err(storage_error)?;
        }

        if !changed_indexes.is_empty() {
            let delete_targets = changed_indexes
                .iter()
                .filter(|(_, _, _, _, known_new)| !known_new)
                .collect::<Vec<_>>();
            if !delete_targets.is_empty() {
                let mut delete_sql =
                    String::from("DELETE FROM entity_field_index WHERE tenant = ? AND (");
                let mut delete_values = Vec::with_capacity(1 + delete_targets.len() * 2);
                delete_values.push(Value::from(tenant.to_string()));
                for (index, (entity_type, entity_id, _, _, _)) in delete_targets.iter().enumerate()
                {
                    if index > 0 {
                        delete_sql.push_str(" OR ");
                    }
                    delete_sql.push_str("(entity_type = ? AND entity_id = ?)");
                    delete_values.push(Value::from(entity_type.clone()));
                    delete_values.push(Value::from(entity_id.clone()));
                }
                delete_sql.push(')');
                tx.execute(&delete_sql, params_from_iter(delete_values))
                    .await
                    .map_err(storage_error)?;
            }

            let indexed_field_count = changed_indexes
                .iter()
                .map(|(_, _, _, fields, _)| fields.len())
                .sum::<usize>();
            if indexed_field_count > 0 {
                let mut insert_sql = String::from(
                    "INSERT INTO entity_field_index \
                     (tenant, entity_type, entity_id, field_name, field_value, status) VALUES ",
                );
                let mut insert_values = Vec::with_capacity(indexed_field_count * 6);
                let mut field_index = 0usize;
                for (entity_type, entity_id, status, indexed_fields, _) in &changed_indexes {
                    for (field_name, field_value) in indexed_fields {
                        if field_index > 0 {
                            insert_sql.push_str(", ");
                        }
                        insert_sql.push_str("(?, ?, ?, ?, ?, ?)");
                        insert_values.push(Value::from(tenant.to_string()));
                        insert_values.push(Value::from(entity_type.clone()));
                        insert_values.push(Value::from(entity_id.clone()));
                        insert_values.push(Value::from(field_name.clone()));
                        insert_values.push(
                            field_value
                                .as_ref()
                                .map(|value| Value::from(value.clone()))
                                .unwrap_or(Value::Null),
                        );
                        insert_values.push(Value::from(status.clone()));
                        field_index += 1;
                    }
                }

                tx.execute(&insert_sql, params_from_iter(insert_values))
                    .await
                    .map_err(storage_error)?;
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Backwards-compatible alias for the old name.
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_field_index",
        tenant, entity_type, entity_id,
    ))]
    pub async fn upsert_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        self.upsert_query_projection(tenant, entity_type, entity_id, status, fields, 0)
            .await
    }

    /// Remove the durable query-plane projection for a single entity.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_query_projection",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.remove_query_projection", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_catalog WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Backwards-compatible alias for the old name.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_field_index",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    /// Query the field index with a pre-built SQL WHERE clause.
    ///
    /// Returns distinct entity IDs matching the filter. The `where_clause`
    /// and `params` are generated by the OData-to-SQL translator
    /// (`filter_sql::try_translate_filter`).
    ///
    /// Parameters use `String` (not `libsql::Value`) so the interface is
    /// usable from crates that don't depend on `libsql` directly.
    #[instrument(skip_all, fields(
        otel.name = "turso.query_field_index",
        tenant, entity_type,
    ))]
    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let sql = format!(
            "SELECT DISTINCT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause})"
        );

        // Prepend the tenant/entity_type params. The translator's params
        // start at ?3 so there's no collision.
        let mut all_params: Vec<libsql::Value> = vec![
            libsql::Value::from(tenant.to_string()),
            libsql::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(libsql::Value::from));

        let mut rows = conn
            .query(&sql, libsql::params_from_iter(all_params))
            .await
            .map_err(storage_error)?;

        let mut ids = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            if let Ok(id) = row.get::<String>(0) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Load a sparse set of projected fields for many entities in one query.
    ///
    /// Returns one row per projected entity that exists in the durable query
    /// plane. Missing entity ids are omitted from the result.
    #[instrument(skip_all, fields(
        otel.name = "turso.load_query_projection_fields_many",
        tenant, entity_type,
        entity_count = entity_ids.len(),
        field_count = field_names.len(),
    ))]
    pub async fn load_query_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Vec<ProjectedEntityFieldsRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.configured_connection().await?;
        let requested_fields: BTreeSet<&str> = field_names.iter().copied().collect();

        let entity_placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let field_placeholders = std::iter::repeat_n("?", requested_fields.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.entity_id, c.status, f.field_name, f.field_value \
             FROM entity_catalog c \
             LEFT JOIN entity_field_index f \
               ON c.tenant = f.tenant \
              AND c.entity_type = f.entity_type \
              AND c.entity_id = f.entity_id \
              AND f.field_name IN ({field_placeholders}) \
             WHERE c.tenant = ? \
               AND c.entity_type = ? \
               AND c.entity_id IN ({entity_placeholders}) \
             ORDER BY c.entity_id, f.field_name"
        );

        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(2 + requested_fields.len() + entity_ids.len());
        for field_name in &requested_fields {
            params.push(libsql::Value::from((*field_name).to_string()));
        }
        params.push(libsql::Value::from(tenant.to_string()));
        params.push(libsql::Value::from(entity_type.to_string()));
        for entity_id in entity_ids {
            params.push(libsql::Value::from(entity_id.clone()));
        }

        let mut rows = conn
            .query(&sql, libsql::params_from_iter(params))
            .await
            .map_err(storage_error)?;

        let mut by_entity: BTreeMap<String, ProjectedEntityFieldsRow> = BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let field_name: Option<String> = row.get(2).map_err(storage_error)?;
            let field_value: Option<String> = row.get(3).map_err(storage_error)?;

            let entry =
                by_entity
                    .entry(entity_id.clone())
                    .or_insert_with(|| ProjectedEntityFieldsRow {
                        entity_id: entity_id.clone(),
                        status,
                        fields: requested_fields
                            .iter()
                            .map(|field| ((*field).to_string(), None))
                            .collect(),
                    });

            if let Some(field_name) = field_name
                && requested_fields.contains(field_name.as_str())
            {
                entry.fields.insert(field_name, field_value);
            }
        }

        Ok(entity_ids
            .iter()
            .filter_map(|entity_id| by_entity.remove(entity_id))
            .collect())
    }

    /// Export every durable query-plane projection, optionally scoped to one tenant.
    ///
    /// Turso keeps projected fields in `entity_field_index`, so this reconstructs
    /// the full projected fields JSON preserved in the catalog.
    #[instrument(skip_all, fields(
        otel.name = "turso.export_query_projections",
        tenant = tenant.unwrap_or("*"),
    ))]
    pub async fn export_query_projections(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<TursoQueryProjectionRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, status, fields, sequence_nr \
                 FROM entity_catalog \
                 WHERE (?1 IS NULL OR tenant = ?1) \
                 ORDER BY tenant, entity_type, entity_id",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant: String = row.get(0).map_err(storage_error)?;
            let entity_type: String = row.get(1).map_err(storage_error)?;
            let entity_id: String = row.get(2).map_err(storage_error)?;
            let status: String = row.get(3).map_err(storage_error)?;
            let fields_text: String = row.get(4).map_err(storage_error)?;
            let sequence_nr = row.get::<i64>(5).map_err(storage_error)?.max(0) as u64;
            let fields = serde_json::from_str(&fields_text)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            out.push(TursoQueryProjectionRow {
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                sequence_nr,
            });
        }

        Ok(out)
    }

    /// Return projected entity counts grouped by tenant.
    #[instrument(skip_all, fields(otel.name = "turso.projected_entity_counts_by_tenant"))]
    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Vec<(String, u64)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, COUNT(*) AS projected_count \
                 FROM entity_catalog \
                 GROUP BY tenant \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut counts = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant: String = row.get(0).map_err(storage_error)?;
            let count: i64 = row.get(1).map_err(storage_error)?;
            counts.push((tenant, count.max(0) as u64));
        }
        Ok(counts)
    }

    /// Batch-load full entity catalog rows for a list of entity IDs.
    ///
    /// Returns only rows that exist in the catalog; missing IDs are silently
    /// omitted. Mirrors the Postgres impl so the OData read path can stay
    /// backend-agnostic.
    #[instrument(skip_all, fields(
        otel.name = "turso.load_entity_catalog_rows",
        tenant, entity_type,
        entity_count = entity_ids.len(),
    ))]
    pub async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<EntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT entity_id, status, fields, sequence_nr \
             FROM entity_catalog \
             WHERE tenant = ? AND entity_type = ? AND entity_id IN ({placeholders}) \
             ORDER BY entity_id"
        );
        let mut params: Vec<libsql::Value> = Vec::with_capacity(entity_ids.len() + 2);
        params.push(libsql::Value::Text(tenant.to_string()));
        params.push(libsql::Value::Text(entity_type.to_string()));
        for id in entity_ids {
            params.push(libsql::Value::Text(id.clone()));
        }
        let mut rows = conn.query(&sql, params).await.map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let fields_text: String = row.get(2).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(3).map_err(storage_error)?;
            let fields: serde_json::Value = serde_json::from_str(&fields_text)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            out.push(EntityCatalogRow {
                entity_id,
                status,
                fields,
                sequence_nr: sequence_nr.max(0) as u64,
            });
        }
        Ok(out)
    }
}

/// Convert a JSON scalar to a TEXT representation for the index.
///
/// Returns `None` for non-scalar types (objects, arrays) — these are not indexed.
/// `null` values return `None` (stored as SQL NULL in field_value).
fn scalar_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

fn indexed_projection_fields(
    status: &str,
    fields: &serde_json::Value,
) -> Vec<(String, Option<String>)> {
    let mut indexed_fields = Vec::new();

    if let Some(obj) = fields.as_object() {
        for (field_name, value) in obj {
            if field_name == "Status" {
                continue;
            }

            let field_value = scalar_to_text(value);
            if field_value.is_none() && !value.is_null() {
                continue;
            }
            if field_value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_INDEXABLE_FIELD_VALUE_BYTES)
            {
                continue;
            }

            indexed_fields.push((field_name.clone(), field_value));
        }
    }

    // Also index the status as a pseudo-field so `$filter=Status eq 'Active'` works.
    indexed_fields.push(("Status".to_string(), Some(status.to_string())));
    indexed_fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_to_text_converts_primitives() {
        assert_eq!(
            scalar_to_text(&serde_json::json!("hello")),
            Some("hello".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(42)),
            Some("42".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(true)),
            Some("true".to_string())
        );
        assert_eq!(scalar_to_text(&serde_json::Value::Null), None);
    }

    #[test]
    fn scalar_to_text_skips_complex_types() {
        assert_eq!(scalar_to_text(&serde_json::json!({"a": 1})), None);
        assert_eq!(scalar_to_text(&serde_json::json!([1, 2, 3])), None);
    }

    #[test]
    fn indexed_projection_fields_skips_oversized_scalars() {
        let long = "x".repeat(MAX_INDEXABLE_FIELD_VALUE_BYTES + 1);
        let fields = serde_json::json!({
            "Title": "short",
            "Payload": long,
        });

        let indexed = indexed_projection_fields("Active", &fields);

        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Title" && value.as_deref() == Some("short"))
        );
        assert!(indexed.iter().all(|(name, _)| name != "Payload"));
        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Status" && value.as_deref() == Some("Active"))
        );
    }
}
