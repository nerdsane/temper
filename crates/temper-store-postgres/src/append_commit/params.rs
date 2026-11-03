//! Parameter preparation and binding for the append statement.

use super::{AppendAnswer, check_distinct_key_names, check_same_record, stored_sequence};
use crate::platform::{canonical_projection_status, json_hash, scalar_index_fields};
use temper_runtime::persistence::{
    CommitRows, PersistenceEnvelope, PersistenceError, ProjectionRow, StateTimerRow,
    catalog_state_for_storage, pack_f32_le,
};

/// One append's parameters, owned and laid out as the statement's arrays.
pub(super) struct AppendParams<'a> {
    record: [&'a str; 3],
    expected_sequence: u64,
    event_types: Vec<&'a str>,
    payloads: Vec<&'a serde_json::Value>,
    metadata: Vec<serde_json::Value>,
    tombstone: bool,
    key_names: Vec<&'a str>,
    key_hashes: Vec<&'a str>,
    reconcile_vectors: bool,
    vector_decls: Vec<&'a str>,
    vector_models: Vec<&'a str>,
    vectors: Vec<Vec<u8>>,
    arm_ids: Vec<&'a str>,
    arm_actions: Vec<&'a str>,
    arm_due_at: Vec<&'a str>,
    arm_armed_by: Vec<&'a str>,
    fired: Option<&'a str>,
    pub(super) projection: ProjectionParams<'a>,
    timer: TimerParams<'a>,
}

/// The catalog and field-index rows, as the projection writer computes them.
#[derive(Default)]
pub(super) struct ProjectionParams<'a> {
    pub(super) op: Option<&'static str>,
    status: &'a str,
    fields: Option<&'a serde_json::Value>,
    stored_state: serde_json::Value,
    hash: String,
    index_names: Vec<String>,
    index_values: Vec<String>,
    wrapped: bool,
    pub(super) indexed_fields: u64,
    pub(super) skipped_fields: u64,
}

/// The timer row: `arm` with its row, `end`, or nothing.
#[derive(Default)]
struct TimerParams<'a> {
    op: Option<&'static str>,
    state: &'a str,
    arm_id: &'a str,
    sequence_nr: i64,
    started_at: &'a str,
    params: &'a str,
    armed_by: &'a str,
}

impl<'a> AppendParams<'a> {
    /// Lay out `rows` for the statement.
    ///
    /// # Errors
    ///
    /// A row of another record, or a declared key named twice: the statement
    /// would write it under this record, or change one key row twice.
    pub(super) fn new(
        record: [&'a str; 3],
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        rows: CommitRows<'a>,
    ) -> Result<Self, PersistenceError> {
        let metadata = events
            .iter()
            .map(|event| serde_json::to_value(&event.metadata))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        check_distinct_key_names(rows.key_rows)?;
        for action in rows.scheduled_actions {
            check_same_record(
                record,
                [&action.tenant, &action.entity_type, &action.entity_id],
            )?;
        }
        let vector_rows = if rows.reconcile_vectors {
            rows.vector_rows
        } else {
            &[]
        };
        let params = Self {
            record,
            expected_sequence,
            event_types: events.iter().map(|e| e.event_type.as_str()).collect(),
            payloads: events.iter().map(|e| &e.payload).collect(),
            metadata,
            tombstone: events.iter().any(PersistenceEnvelope::is_tombstone),
            key_names: rows.key_rows.iter().map(|k| k.key_name.as_str()).collect(),
            key_hashes: rows.key_rows.iter().map(|k| k.key_hash.as_str()).collect(),
            reconcile_vectors: rows.reconcile_vectors,
            vector_decls: vector_rows.iter().map(|v| v.decl_name.as_str()).collect(),
            vector_models: vector_rows.iter().map(|v| v.model_tag.as_str()).collect(),
            vectors: vector_rows.iter().map(|v| pack_f32_le(&v.vector)).collect(),
            arm_ids: rows
                .scheduled_actions
                .iter()
                .map(|a| a.arm_id.as_str())
                .collect(),
            arm_actions: rows
                .scheduled_actions
                .iter()
                .map(|a| a.action.as_str())
                .collect(),
            arm_due_at: rows
                .scheduled_actions
                .iter()
                .map(|a| a.due_at.as_str())
                .collect(),
            arm_armed_by: rows
                .scheduled_actions
                .iter()
                .map(|a| a.armed_by.as_str())
                .collect(),
            fired: rows.fired_scheduled_action,
            projection: ProjectionParams::new(rows.projection),
            timer: TimerParams::new(record, rows.state_timer)?,
        };
        debug_assert_eq!(params.event_types.len(), params.metadata.len());
        debug_assert!(params.index_lengths_match());
        Ok(params)
    }

    fn index_lengths_match(&self) -> bool {
        self.key_names.len() == self.key_hashes.len()
            && self.vector_decls.len() == self.vectors.len()
            && self.arm_ids.len() == self.arm_armed_by.len()
            && self.projection.index_names.len() == self.projection.index_values.len()
    }
}

impl<'a> ProjectionParams<'a> {
    fn new(projection: Option<ProjectionRow<'a>>) -> Self {
        match projection {
            None => Self::default(),
            Some(ProjectionRow::Remove) => Self {
                op: Some("remove"),
                ..Self::default()
            },
            Some(ProjectionRow::Upsert {
                status,
                fields,
                state,
            }) => {
                let (index, indexed_fields, skipped_fields) = scalar_index_fields(fields);
                let (index_names, index_values) = index.into_iter().unzip();
                Self {
                    op: Some("upsert"),
                    status: canonical_projection_status(status, state),
                    fields: Some(fields),
                    // The row holds each field once: in `fields`, not again in `state`.
                    stored_state: catalog_state_for_storage(fields, state),
                    hash: json_hash(fields),
                    index_names,
                    index_values,
                    // The field index does not look inside wrapped legacy
                    // fields, so such a record stays in the gap.
                    wrapped: fields
                        .get("fields")
                        .is_some_and(serde_json::Value::is_object),
                    indexed_fields,
                    skipped_fields,
                }
            }
        }
    }
}

impl<'a> TimerParams<'a> {
    fn new(
        record: [&'a str; 3],
        timer: Option<StateTimerRow<'a>>,
    ) -> Result<Self, PersistenceError> {
        Ok(match timer {
            None => Self::default(),
            Some(StateTimerRow::End) => Self {
                op: Some("end"),
                ..Self::default()
            },
            Some(StateTimerRow::Arm(row)) => {
                check_same_record(record, [&row.tenant, &row.entity_type, &row.entity_id])?;
                Self {
                    op: Some("arm"),
                    state: &row.state,
                    arm_id: &row.arm_id,
                    sequence_nr: stored_sequence(row.sequence_nr),
                    started_at: &row.started_at,
                    params: &row.params,
                    armed_by: &row.armed_by,
                }
            }
        })
    }
}

impl AppendParams<'_> {
    /// Bind every parameter to `query`, in the statement's order.
    pub(super) fn bind(
        &self,
        query: crate::dbm::PostgresQueryAs<AppendAnswer>,
    ) -> crate::dbm::PostgresQueryAs<AppendAnswer> {
        let [tenant, entity_type, entity_id] = self.record;
        let projection = &self.projection;
        let timer = &self.timer;
        query
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(stored_sequence(self.expected_sequence))
            .bind(&self.event_types)
            .bind(&self.payloads)
            .bind(&self.metadata)
            .bind(&self.key_names)
            .bind(&self.key_hashes)
            .bind(self.reconcile_vectors)
            .bind(&self.vector_decls)
            .bind(&self.vector_models)
            .bind(&self.vectors)
            .bind(&self.arm_ids)
            .bind(&self.arm_actions)
            .bind(&self.arm_due_at)
            .bind(&self.arm_armed_by)
            .bind(self.fired)
            .bind(projection.op)
            .bind(projection.status)
            .bind(projection.fields.unwrap_or(&serde_json::Value::Null))
            .bind(&projection.stored_state)
            .bind(projection.hash.as_str())
            .bind(&projection.index_names)
            .bind(&projection.index_values)
            .bind(projection.wrapped)
            .bind(self.tombstone)
            .bind(timer.op)
            .bind(timer.state)
            .bind(timer.arm_id)
            .bind(timer.sequence_nr)
            .bind(timer.started_at)
            .bind(timer.params)
            .bind(timer.armed_by)
    }
}
