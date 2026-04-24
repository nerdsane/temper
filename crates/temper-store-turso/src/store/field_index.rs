//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use libsql::{TransactionBehavior, params};
use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::TursoEventStore;

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
        let conn = self.configured_connection().await?;
        let new_projection_hash = projection_hash(status, fields);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let mut existing_rows = tx
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
            tx.execute(
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
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }

        tx.execute(
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
        tx.execute(
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
                tx.execute(
                    "INSERT INTO entity_field_index (tenant, entity_type, entity_id, field_name, field_value, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![tenant, entity_type, entity_id, field_name.as_str(), field_value, status],
                )
                .await
                .map_err(storage_error)?;
            }
        }

        // Also index the status as a pseudo-field so `$filter=Status eq 'Active'` works.
        tx.execute(
            "INSERT OR REPLACE INTO entity_field_index (tenant, entity_type, entity_id, field_name, field_value, status) \
             VALUES (?1, ?2, ?3, 'Status', ?4, ?4)",
            params![tenant, entity_type, entity_id, status],
        )
        .await
        .map_err(storage_error)?;

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
