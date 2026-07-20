//! Durable query projection upserts.

use super::*;

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
        let _write_permit = self
            .acquire_write_permit("turso.upsert_query_projection", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let new_projection_hash = projection_hash(status, fields);
        let fields_json = serde_json::to_string(fields)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let state_json = serde_json::to_string(state)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let updated_at = sim_now().to_rfc3339();
        let sequence_nr = i64::try_from(sequence_nr).unwrap_or(i64::MAX);

        let unchanged_rows = conn
            .execute(
                "UPDATE entity_catalog \
                 SET status = ?4, updated_at = ?5, sequence_nr = ?6, projection_version = 2, fields = ?8, state = ?9 \
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
                    state_json.as_str(),
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

        let delete_sequence = {
            let mut rows = tx
                .query(
                    "SELECT sequence_nr FROM query_projection_tombstones \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            let sequence = if let Some(row) = rows.next().await.map_err(storage_error)? {
                Some(row.get::<i64>(0).map_err(storage_error)?)
            } else {
                None
            };
            drop(rows);
            sequence
        };
        if delete_sequence.is_some_and(|deleted| deleted >= sequence_nr) {
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }

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
             (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 2, ?9) \
             ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 status = excluded.status, \
                 fields = excluded.fields, \
                 state = excluded.state, \
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
                state_json.as_str(),
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
        let existing_candidates = projections.iter().collect::<Vec<_>>();
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

        let mut delete_high_waters = BTreeMap::new();
        let mut tombstone_sql = String::from(
            "SELECT entity_type, entity_id, sequence_nr \
             FROM query_projection_tombstones WHERE tenant = ? AND (",
        );
        let mut tombstone_values = Vec::with_capacity(1 + projections.len() * 2);
        tombstone_values.push(Value::from(tenant.to_string()));
        for (index, projection) in projections.iter().enumerate() {
            if index > 0 {
                tombstone_sql.push_str(" OR ");
            }
            tombstone_sql.push_str("(entity_type = ? AND entity_id = ?)");
            tombstone_values.push(Value::from(projection.entity_type.clone()));
            tombstone_values.push(Value::from(projection.entity_id.clone()));
        }
        tombstone_sql.push(')');
        let mut tombstone_rows = tx
            .query(&tombstone_sql, params_from_iter(tombstone_values))
            .await
            .map_err(storage_error)?;
        while let Some(row) = tombstone_rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let entity_id: String = row.get(1).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(2).map_err(storage_error)?;
            delete_high_waters.insert((entity_type, entity_id), sequence_nr);
        }
        drop(tombstone_rows);

        struct PreparedProjection {
            entity_type: String,
            entity_id: String,
            status: String,
            fields_json: String,
            state_json: String,
            sequence_nr: i64,
            projection_hash: String,
        }

        let mut catalog_upserts = Vec::new();
        let mut changed_indexes = Vec::new();
        for projection in projections {
            let new_projection_hash = projection_hash(&projection.status, &projection.fields);
            let fields_json = serde_json::to_string(&projection.fields)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let state_json = serde_json::to_string(&projection.state)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let sequence_nr = i64::try_from(projection.sequence_nr).unwrap_or(i64::MAX);
            let key = (projection.entity_type.clone(), projection.entity_id.clone());
            if delete_high_waters
                .get(&key)
                .is_some_and(|deleted_sequence| *deleted_sequence >= sequence_nr)
            {
                continue;
            }
            let existing_row = existing.get(&key);
            if existing_row.is_some_and(|(existing_sequence, _)| *existing_sequence > sequence_nr) {
                continue;
            }
            if existing_row.is_none_or(|(_, existing_hash)| existing_hash != &new_projection_hash) {
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
                state_json,
                sequence_nr,
                projection_hash: new_projection_hash,
            });
        }

        if !catalog_upserts.is_empty() {
            let mut upsert_sql = String::from(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
                 VALUES ",
            );
            let mut upsert_values = Vec::with_capacity(catalog_upserts.len() * 9);
            for (index, projection) in catalog_upserts.iter().enumerate() {
                if index > 0 {
                    upsert_sql.push_str(", ");
                }
                upsert_sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, 2, ?)");
                upsert_values.push(Value::from(tenant.to_string()));
                upsert_values.push(Value::from(projection.entity_type.clone()));
                upsert_values.push(Value::from(projection.entity_id.clone()));
                upsert_values.push(Value::from(projection.status.clone()));
                upsert_values.push(Value::from(projection.fields_json.clone()));
                upsert_values.push(Value::from(projection.state_json.clone()));
                upsert_values.push(Value::from(updated_at.clone()));
                upsert_values.push(Value::from(projection.sequence_nr));
                upsert_values.push(Value::from(projection.projection_hash.clone()));
            }
            upsert_sql.push_str(
                " ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 status = excluded.status, \
                 fields = excluded.fields, \
                 state = excluded.state, \
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
}
