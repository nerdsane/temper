//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use crate::driver::{Value, params, params_from_iter};
use std::collections::{BTreeMap, BTreeSet};
use temper_runtime::persistence::{
    PersistenceError, catalog_state_for_storage, catalog_state_from_storage, storage_error,
};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::field_index_rows::sync_field_index;
use super::projection_rows::{
    ProjectionWrite, existing_projection, remove_projection, write_projection,
};
use super::unchanged_projections::{
    apply_event_counters, drop_event_counters, projection_hash, store_event_counters,
};
use super::write_gate::WritePriority;
use super::{TursoEventStore, TursoQueryProjectionRow};

/// Match the Postgres query-plane limit: large scalar values stay in
/// `entity_catalog.fields` but are not copied into the filter index.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;

pub(super) fn canonical_projection_status<'a>(
    fallback: &'a str,
    state: &'a serde_json::Value,
) -> &'a str {
    state
        .get("status")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
        .unwrap_or(fallback)
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
    pub state: Option<serde_json::Value>,
    pub sequence_nr: u64,
}

/// One durable projection upsert in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryProjectionUpsert {
    pub entity_type: String,
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: serde_json::Value,
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
        let conn = self
            .write_connection("turso.upsert_query_projection", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        write_projection(
            &tx,
            &ProjectionWrite {
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                state,
                sequence_nr,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Rewrite an existing catalog row and its field index from `state`, as
    /// [`Self::upsert_query_projection_with_state`] does, and leave a record
    /// with no catalog row alone: while the server serves, only the record's
    /// own commit creates its row (boot repair creates the missing ones before
    /// serving). A record deleted after `state` was read stays deleted.
    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    pub async fn refresh_query_projection_with_state(
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
        let conn = self
            .write_connection("turso.refresh_query_projection", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        if existing_projection(&tx, [tenant, entity_type, entity_id])
            .await?
            .is_some()
        {
            write_projection(
                &tx,
                &ProjectionWrite {
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    fields,
                    state,
                    sequence_nr,
                },
            )
            .await?;
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

        // Decide once per entity, against its newest projection. Comparing each
        // entry with the original row can stage older content while a newer,
        // unchanged entry advances only the counters.
        let mut newest = BTreeMap::new();
        for projection in projections {
            let key = (&projection.entity_type, &projection.entity_id);
            let current = newest.entry(key).or_insert(projection);
            if projection.sequence_nr >= current.sequence_nr {
                *current = projection;
            }
        }
        let projections = newest.into_values().collect::<Vec<_>>();

        let conn = self
            .write_connection("turso.upsert_query_projections", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        let updated_at = sim_now().to_rfc3339();
        // One lookup by key per entity: a statement that ORs the keys
        // together can only search by tenant, reading its whole catalog.
        let mut existing = BTreeMap::new();
        for projection in projections
            .iter()
            .filter(|projection| !projection.known_new)
        {
            let key = [
                tenant,
                projection.entity_type.as_str(),
                projection.entity_id.as_str(),
            ];
            if let Some(found) = existing_projection(&tx, key).await? {
                existing.insert(
                    (projection.entity_type.clone(), projection.entity_id.clone()),
                    found,
                );
            }
        }

        struct PreparedProjection {
            entity_type: String,
            entity_id: String,
            status: String,
            fields_json: String,
            state_json: String,
            first_event_at: Option<String>,
            sequence_nr: i64,
            projection_hash: String,
        }

        let mut catalog_upserts = Vec::new();
        let mut changed_indexes = Vec::new();
        let mut applied_projections = Vec::new();
        for projection in &projections {
            let fields_json = serde_json::to_string(&projection.fields)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let stored_state = catalog_state_for_storage(&projection.fields, &projection.state);
            let state_json = serde_json::to_string(&stored_state)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let new_projection_hash =
                projection_hash(&projection.status, &fields_json, &stored_state);
            let sequence_nr = i64::try_from(projection.sequence_nr).unwrap_or(i64::MAX);
            let existing_row =
                existing.get(&(projection.entity_type.clone(), projection.entity_id.clone()));
            if existing_row.is_some_and(|existing| existing.sequence_nr() > sequence_nr) {
                continue;
            }
            applied_projections.push(*projection);
            let entity = [
                tenant,
                projection.entity_type.as_str(),
                projection.entity_id.as_str(),
            ];
            if let Some(existing) =
                existing_row.filter(|existing| existing.projection_hash == new_projection_hash)
            {
                if sequence_nr > existing.sequence_nr() {
                    store_event_counters(&tx, entity, sequence_nr, &projection.state).await?;
                }
                continue;
            }
            if existing_row.is_some_and(|existing| existing.counters_sequence_nr.is_some()) {
                drop_event_counters(&tx, entity).await?;
            }
            changed_indexes.push((
                projection.entity_type.clone(),
                projection.entity_id.clone(),
                indexed_projection_fields(&projection.status, &projection.indexed_fields),
                projection.known_new,
            ));
            catalog_upserts.push(PreparedProjection {
                entity_type: projection.entity_type.clone(),
                entity_id: projection.entity_id.clone(),
                status: projection.status.clone(),
                fields_json,
                state_json,
                first_event_at: projection
                    .state
                    .get("first_event_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                sequence_nr,
                projection_hash: new_projection_hash,
            });
        }

        if !catalog_upserts.is_empty() {
            let mut upsert_sql = String::from(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash, first_event_at) \
                 VALUES ",
            );
            let mut upsert_values = Vec::with_capacity(catalog_upserts.len() * 10);
            for (index, projection) in catalog_upserts.iter().enumerate() {
                if index > 0 {
                    upsert_sql.push_str(", ");
                }
                upsert_sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, 2, ?, ?)");
                upsert_values.push(Value::from(tenant.to_string()));
                upsert_values.push(Value::from(projection.entity_type.clone()));
                upsert_values.push(Value::from(projection.entity_id.clone()));
                upsert_values.push(Value::from(projection.status.clone()));
                upsert_values.push(Value::from(projection.fields_json.clone()));
                upsert_values.push(Value::from(projection.state_json.clone()));
                upsert_values.push(Value::from(updated_at.clone()));
                upsert_values.push(Value::from(projection.sequence_nr));
                upsert_values.push(Value::from(projection.projection_hash.clone()));
                upsert_values.push(
                    projection
                        .first_event_at
                        .clone()
                        .map(Value::Text)
                        .unwrap_or(Value::Null),
                );
            }
            upsert_sql.push_str(
                " ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 status = excluded.status, \
                 fields = excluded.fields, \
                 state = excluded.state, \
                 first_event_at = COALESCE(entity_catalog.first_event_at, excluded.first_event_at), \
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

        // An entity that may already have rows writes only the ones that
        // changed. A known-new entity has none, so its rows go in one insert.
        let (new_indexes, existing_indexes): (Vec<_>, Vec<_>) = changed_indexes
            .into_iter()
            .partition(|(_, _, _, known_new)| *known_new);
        for (entity_type, entity_id, indexed_fields, _) in &existing_indexes {
            sync_field_index(
                &tx,
                [tenant, entity_type.as_str(), entity_id.as_str()],
                indexed_fields,
            )
            .await?;
        }
        let changed_indexes = new_indexes;
        if !changed_indexes.is_empty() {
            let indexed_field_count = changed_indexes
                .iter()
                .map(|(_, _, fields, _)| fields.len())
                .sum::<usize>();
            if indexed_field_count > 0 {
                let mut insert_sql = String::from(
                    "INSERT INTO entity_field_index \
                     (tenant, entity_type, entity_id, field_name, field_value) VALUES ",
                );
                let mut insert_values = Vec::with_capacity(indexed_field_count * 5);
                let mut field_index = 0usize;
                for (entity_type, entity_id, indexed_fields, _) in &changed_indexes {
                    for (field_name, field_value) in indexed_fields {
                        if field_index > 0 {
                            insert_sql.push_str(", ");
                        }
                        insert_sql.push_str("(?, ?, ?, ?, ?)");
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
                        field_index += 1;
                    }
                }

                tx.execute(&insert_sql, params_from_iter(insert_values))
                    .await
                    .map_err(storage_error)?;
            }
        }

        for projection in applied_projections {
            super::projection_delta::applied(
                &tx,
                [tenant, &projection.entity_type, &projection.entity_id],
                projection.sequence_nr,
                &projection.fields,
            )
            .await?;
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
            "total_event_count": 0,
            "sequence_nr": 0
        });
        self.upsert_query_projection_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            0,
        )
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
        let conn = self
            .write_connection("turso.remove_query_projection", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        remove_projection(&tx, [tenant, entity_type, entity_id]).await?;
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
    /// Returns the IDs of entities matching the filter. The `where_clause`
    /// and `params` are generated by the OData-to-SQL translator
    /// (`filter_sql::try_translate_filter`). As in Postgres, the clause runs
    /// over the entity's catalog row: `status` is the catalog's status, and a
    /// field is matched with `entity_id IN (SELECT entity_id FROM
    /// entity_field_index WHERE ...)`.
    ///
    /// Parameters use `String` (not `crate::driver::Value`) so the interface is
    /// usable from crates that don't depend on the database driver directly.
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
        let index = catalog_filter_index(where_clause);
        let sql = format!(
            "SELECT entity_id FROM entity_catalog {index}\
             WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause})"
        );

        // Prepend the tenant/entity_type params. The translator's params
        // start at ?3 so there's no collision.
        let mut all_params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(crate::driver::Value::from));

        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(all_params))
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
        // Each record's field rows are found by its key: the engine otherwise
        // searches `idx_efi_status` by tenant and type, reading every field
        // row of the type for each record (`driver/plan_check.rs`).
        let sql = format!(
            "SELECT c.entity_id, c.status, f.field_name, f.field_value \
             FROM entity_catalog c \
             LEFT JOIN entity_field_index f INDEXED BY sqlite_autoindex_entity_field_index_1 \
               ON c.tenant = f.tenant \
              AND c.entity_type = f.entity_type \
              AND c.entity_id = f.entity_id \
              AND f.field_name IN ({field_placeholders}) \
             WHERE c.tenant = ? \
               AND c.entity_type = ? \
               AND c.entity_id IN ({entity_placeholders}) \
             ORDER BY c.entity_id, f.field_name"
        );

        let mut params: Vec<crate::driver::Value> =
            Vec::with_capacity(2 + requested_fields.len() + entity_ids.len());
        for field_name in &requested_fields {
            params.push(crate::driver::Value::from((*field_name).to_string()));
        }
        params.push(crate::driver::Value::from(tenant.to_string()));
        params.push(crate::driver::Value::from(entity_type.to_string()));
        for entity_id in entity_ids {
            params.push(crate::driver::Value::from(entity_id.clone()));
        }

        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(params))
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
                "SELECT c.tenant, c.entity_type, c.entity_id, c.status, c.fields, \
                        COALESCE(k.sequence_nr, c.sequence_nr) \
                 FROM entity_catalog c LEFT JOIN entity_catalog_counters k \
                   ON k.tenant = c.tenant AND k.entity_type = c.entity_type \
                  AND k.entity_id = c.entity_id \
                 WHERE (?1 IS NULL OR c.tenant = ?1) \
                 ORDER BY c.tenant, c.entity_type, c.entity_id",
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
    /// backend-agnostic. Each row's `state` comes back whole, its fields put
    /// back from the `fields` column ([`catalog_state_from_storage`]).
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
            "SELECT c.entity_id, c.status, c.fields, c.state, \
                    COALESCE(k.sequence_nr, c.sequence_nr), k.counters, c.first_event_at \
             FROM entity_catalog c LEFT JOIN entity_catalog_counters k \
               ON k.tenant = c.tenant AND k.entity_type = c.entity_type \
              AND k.entity_id = c.entity_id \
             WHERE c.tenant = ? AND c.entity_type = ? AND c.entity_id IN ({placeholders}) \
             ORDER BY c.entity_id"
        );
        let mut params: Vec<crate::driver::Value> = Vec::with_capacity(entity_ids.len() + 2);
        params.push(crate::driver::Value::Text(tenant.to_string()));
        params.push(crate::driver::Value::Text(entity_type.to_string()));
        for id in entity_ids {
            params.push(crate::driver::Value::Text(id.clone()));
        }
        let mut rows = conn.query(&sql, params).await.map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let fields_text: String = row.get(2).map_err(storage_error)?;
            let state_text: Option<String> = row.get(3).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(4).map_err(storage_error)?;
            let counters: Option<String> = row.get(5).map_err(storage_error)?;
            let fields: serde_json::Value = serde_json::from_str(&fields_text)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let mut state = state_text
                .map(|state_text| serde_json::from_str(&state_text))
                .transpose()
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?
                .map(|stored| catalog_state_from_storage(&fields, stored));
            let first_event_at: Option<String> = row.get(6).map_err(storage_error)?;
            if state.is_none() && first_event_at.is_some() {
                state = Some(serde_json::json!({}));
            }
            if let Some(stored) = state.as_mut().and_then(serde_json::Value::as_object_mut)
                && (first_event_at.is_some() || stored.contains_key("first_event_at"))
            {
                stored.insert("first_event_at".into(), serde_json::json!(first_event_at));
            }
            // A newer unchanged projection stored only its counters; serve
            // the row as that projection would have written it.
            if let Some(counters) = counters {
                apply_event_counters(state.as_mut(), &counters)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            }
            out.push(EntityCatalogRow {
                entity_id,
                status,
                fields,
                state,
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

pub(super) fn indexed_projection_fields(
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

/// What a `$filter` over the catalog reads it by. Every one fixes the tenant
/// and type; one that also fixes the status (`status = ?`) reads
/// `idx_entity_catalog_status` by all three, which turso_core's planner does
/// not choose by itself (see `driver/plan_check.rs`). Without one, the
/// planner's walk of the primary key also keeps a page in `entity_id` order.
pub(super) fn catalog_filter_index(where_clause: &str) -> &'static str {
    if crate::sql_terms::gives(where_clause, "entity_catalog", true, "status") {
        "INDEXED BY idx_entity_catalog_status "
    } else {
        ""
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
