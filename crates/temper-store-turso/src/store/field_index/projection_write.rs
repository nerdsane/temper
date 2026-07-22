//! Source-fenced query-projection writes.

use super::*;

impl TursoEventStore {
    /// Upsert the durable query-plane projection for a single entity.
    ///
    /// Maintains both `entity_catalog` and the EAV `entity_field_index`.
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
        self.upsert_query_projection_with_state_inner(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Upsert a projection only while its exact journal/snapshot source is current.
    #[expect(
        clippy::too_many_arguments,
        reason = "source-fenced projection boundary"
    )]
    pub async fn upsert_query_projection_with_state_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        self.upsert_query_projection_with_state_inner(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
            Some(source),
        )
        .await
    }

    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    async fn upsert_query_projection_with_state_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
        source: Option<ProjectionSourceFence<'_>>,
    ) -> Result<bool, PersistenceError> {
        let source_fenced = source.is_some();
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

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let journal_sequence = {
            let mut journal_rows = tx
                .query(
                    "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            journal_rows
                .next()
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<i64>(0).map_err(storage_error))
                .transpose()?
                .unwrap_or(0)
        };
        let source_backed = if journal_sequence > 0 {
            true
        } else {
            let mut rows = tx
                .query(
                    "SELECT EXISTS(SELECT 1 FROM snapshots \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3)",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            rows.next()
                .await
                .map_err(storage_error)?
                .map(|row| {
                    row.get::<i64>(0)
                        .map(|exists| exists != 0)
                        .map_err(storage_error)
                })
                .transpose()?
                .unwrap_or(false)
        };

        if let Some(source) = source {
            let journal_matches =
                u64::try_from(journal_sequence).ok() == Some(source.expected_journal_sequence);
            let current_snapshot = {
                let mut snapshot_rows = tx
                    .query(
                        "SELECT sequence_nr, snapshot FROM snapshots \
                         WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                        params![tenant, entity_type, entity_id],
                    )
                    .await
                    .map_err(storage_error)?;
                snapshot_rows
                    .next()
                    .await
                    .map_err(storage_error)?
                    .map(|row| {
                        Ok::<_, PersistenceError>((
                            row.get::<i64>(0).map_err(storage_error)?,
                            row.get::<Vec<u8>>(1).map_err(storage_error)?,
                        ))
                    })
                    .transpose()?
            };
            let snapshot_matches = match (source.expected_snapshot, current_snapshot.as_ref()) {
                (None, None) => true,
                (Some(expected), Some((sequence_nr, snapshot))) => {
                    u64::try_from(*sequence_nr).ok() == Some(expected.sequence_nr)
                        && snapshot.as_slice() == expected.state
                }
                _ => false,
            };
            let source_sequence = if source.expected_journal_sequence > 0 {
                source.expected_journal_sequence
            } else {
                source
                    .expected_snapshot
                    .map(|snapshot| snapshot.sequence_nr)
                    .unwrap_or(0)
            };
            if !journal_matches
                || !snapshot_matches
                || u64::try_from(sequence_nr).ok() != Some(source_sequence)
            {
                tx.commit().await.map_err(storage_error)?;
                return Ok(false);
            }
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
        let incoming_is_stale = if source_fenced {
            false
        } else if journal_sequence > 0 {
            sequence_nr != journal_sequence
        } else {
            existing_sequence.is_some_and(|existing| existing > sequence_nr)
        };
        if incoming_is_stale {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }

        let unchanged_rows = tx
            .execute(
                "UPDATE entity_catalog \
                 SET status = ?4, updated_at = ?5, sequence_nr = ?6, projection_version = 2, fields = ?8, state = ?9 \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND projection_hash = ?7",
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
            if source_fenced {
                clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
            } else if source_backed {
                mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
            } else {
                clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(true);
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

        if source_fenced {
            clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        } else if source_backed {
            mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        } else {
            clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(true)
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
}
