//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use libsql::params;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::TursoEventStore;
use crate::retry::{timeout_error, write_attempt_timeout};

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
        let attempt_timeout = write_attempt_timeout();
        match tokio::time::timeout(
            attempt_timeout,
            self.upsert_query_projection_inner(
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                sequence_nr,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(timeout_error(
                "turso.upsert_query_projection",
                attempt_timeout,
            )),
        }
    }

    async fn upsert_query_projection_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        let new_projection_hash = projection_hash(status, fields);

        let mut existing_rows = conn
            .query(
                "SELECT projection_hash FROM entity_catalog \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let existing_projection_hash = existing_rows
            .next()
            .await
            .map_err(storage_error)?
            .and_then(|row| row.get::<String>(0).ok());

        if existing_projection_hash.as_deref() == Some(new_projection_hash.as_str()) {
            conn.execute(
                "UPDATE entity_catalog \
                 SET status = ?4, updated_at = ?5, sequence_nr = ?6, projection_version = 2, projection_hash = ?7 \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    sim_now().to_rfc3339(),
                    i64::try_from(sequence_nr).unwrap_or(i64::MAX),
                    new_projection_hash,
                ],
            )
            .await
            .map_err(storage_error)?;
            return Ok(());
        }

        conn.execute(
            "INSERT OR REPLACE INTO entity_catalog \
             (tenant, entity_type, entity_id, status, updated_at, sequence_nr, projection_version, projection_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2, ?7)",
            params![
                tenant,
                entity_type,
                entity_id,
                status,
                sim_now().to_rfc3339(),
                i64::try_from(sequence_nr).unwrap_or(i64::MAX),
                new_projection_hash,
            ],
        )
        .await
        .map_err(storage_error)?;

        // Delete existing rows for this entity, then re-insert.
        // This is simpler than tracking individual field changes and handles
        // field removal correctly.
        conn.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;

        if let Some(obj) = fields.as_object() {
            for (field_name, value) in obj {
                let field_value = scalar_to_text(value);
                if field_value.is_none() && !value.is_null() {
                    // Non-null, non-scalar (object/array) — skip indexing.
                    continue;
                }
                conn.execute(
                    "INSERT INTO entity_field_index (tenant, entity_type, entity_id, field_name, field_value, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![tenant, entity_type, entity_id, field_name.as_str(), field_value, status],
                )
                .await
                .map_err(storage_error)?;
            }
        }

        // Also index the status as a pseudo-field so `$filter=Status eq 'Active'` works.
        conn.execute(
            "INSERT OR REPLACE INTO entity_field_index (tenant, entity_type, entity_id, field_name, field_value, status) \
             VALUES (?1, ?2, ?3, 'Status', ?4, ?4)",
            params![tenant, entity_type, entity_id, status],
        )
        .await
        .map_err(storage_error)?;

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
        let attempt_timeout = write_attempt_timeout();
        match tokio::time::timeout(
            attempt_timeout,
            self.remove_query_projection_inner(tenant, entity_type, entity_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(timeout_error(
                "turso.remove_query_projection",
                attempt_timeout,
            )),
        }
    }

    async fn remove_query_projection_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM entity_catalog WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
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
}
