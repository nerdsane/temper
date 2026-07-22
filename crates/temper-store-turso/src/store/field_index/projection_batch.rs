//! Batched durable query-projection writes.

use super::*;

impl TursoEventStore {
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
        let mut journal_sequences = BTreeMap::new();
        let mut journal_sql = String::from(
            "SELECT entity_type, entity_id, MAX(sequence_nr) FROM events \
             WHERE tenant = ? AND (",
        );
        let mut journal_values = Vec::with_capacity(1 + projections.len() * 2);
        journal_values.push(Value::from(tenant.to_string()));
        for (index, projection) in projections.iter().enumerate() {
            if index > 0 {
                journal_sql.push_str(" OR ");
            }
            journal_sql.push_str("(entity_type = ? AND entity_id = ?)");
            journal_values.push(Value::from(projection.entity_type.clone()));
            journal_values.push(Value::from(projection.entity_id.clone()));
        }
        journal_sql.push_str(") GROUP BY entity_type, entity_id");
        let mut journal_rows = tx
            .query(&journal_sql, params_from_iter(journal_values))
            .await
            .map_err(storage_error)?;
        while let Some(row) = journal_rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let entity_id: String = row.get(1).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(2).map_err(storage_error)?;
            journal_sequences.insert((entity_type, entity_id), sequence_nr);
        }
        drop(journal_rows);

        let mut snapshot_entities = BTreeSet::new();
        let mut snapshot_sql =
            String::from("SELECT entity_type, entity_id FROM snapshots WHERE tenant = ? AND (");
        let mut snapshot_values = Vec::with_capacity(1 + projections.len() * 2);
        snapshot_values.push(Value::from(tenant.to_string()));
        for (index, projection) in projections.iter().enumerate() {
            if index > 0 {
                snapshot_sql.push_str(" OR ");
            }
            snapshot_sql.push_str("(entity_type = ? AND entity_id = ?)");
            snapshot_values.push(Value::from(projection.entity_type.clone()));
            snapshot_values.push(Value::from(projection.entity_id.clone()));
        }
        snapshot_sql.push(')');
        let mut snapshot_rows = tx
            .query(&snapshot_sql, params_from_iter(snapshot_values))
            .await
            .map_err(storage_error)?;
        while let Some(row) = snapshot_rows.next().await.map_err(storage_error)? {
            snapshot_entities.insert((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        drop(snapshot_rows);

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

        struct PreparedProjection {
            entity_type: String,
            entity_id: String,
            status: String,
            fields_json: String,
            state_json: String,
            sequence_nr: i64,
            projection_hash: String,
            source_backed: bool,
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
            let existing_row = existing.get(&key);
            let journal_sequence = journal_sequences.get(&key).copied().unwrap_or(0);
            let source_backed = journal_sequence > 0 || snapshot_entities.contains(&key);
            let incoming_is_stale = if journal_sequence > 0 {
                sequence_nr != journal_sequence
            } else {
                existing_row.is_some_and(|(existing_sequence, _)| *existing_sequence > sequence_nr)
            };
            if incoming_is_stale {
                continue;
            }
            if existing_row.is_none_or(|(_, existing_hash)| existing_hash != &new_projection_hash) {
                changed_indexes.push((
                    projection.entity_type.clone(),
                    projection.entity_id.clone(),
                    projection.status.clone(),
                    indexed_projection_fields(&projection.status, &projection.indexed_fields),
                    false,
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
                source_backed,
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
                 projection_hash = excluded.projection_hash",
            );
            tx.execute(&upsert_sql, params_from_iter(upsert_values))
                .await
                .map_err(storage_error)?;
        }

        if !changed_indexes.is_empty() {
            let delete_targets = changed_indexes.iter().collect::<Vec<_>>();
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

        for projection in &catalog_upserts {
            if projection.source_backed {
                mark_query_projection_dirty(
                    &tx,
                    tenant,
                    &projection.entity_type,
                    &projection.entity_id,
                )
                .await?;
            } else {
                clear_query_projection_dirty(
                    &tx,
                    tenant,
                    &projection.entity_type,
                    &projection.entity_id,
                )
                .await?;
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }
}
