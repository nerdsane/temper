//! The rows an entity keeps beside its journal, written in its append's
//! transaction: its query-plane row, its state-timer row and its vector rows.
//! An event then never lands without them, and a move pays one commit.

use temper_runtime::persistence::{
    CommitRows, EntityVectorRow, PersistenceError, ProjectionRow, StateTimerRecord, StateTimerRow,
    pack_f32_le, storage_error,
};

use super::super::projection_rows::{ProjectionWrite, remove_projection, write_projection};
use crate::driver::{Transaction, Value, params};

/// An append's entity rows, owned.
#[derive(Debug, Clone, Default)]
pub(super) struct EntityRows {
    projection: Option<Projection>,
    state_timer: Option<TimerRow>,
    /// The entity's vector rows become exactly these (None: leave them).
    vectors: Option<Vec<EntityVectorRow>>,
}

#[derive(Debug, Clone)]
enum Projection {
    Upsert {
        status: String,
        fields: serde_json::Value,
        state: serde_json::Value,
    },
    Remove,
}

#[derive(Debug, Clone)]
enum TimerRow {
    Arm(StateTimerRecord),
    End,
}

impl EntityRows {
    pub(super) fn new(rows: &CommitRows<'_>) -> Self {
        Self {
            projection: rows.projection.map(|projection| match projection {
                ProjectionRow::Upsert {
                    status,
                    fields,
                    state,
                } => Projection::Upsert {
                    status: status.to_string(),
                    fields: fields.clone(),
                    state: state.clone(),
                },
                ProjectionRow::Remove => Projection::Remove,
            }),
            state_timer: rows.state_timer.map(|timer| match timer {
                StateTimerRow::Arm(row) => TimerRow::Arm(row.clone()),
                StateTimerRow::End => TimerRow::End,
            }),
            vectors: rows.reconcile_vectors.then(|| rows.vector_rows.to_vec()),
        }
    }

    /// Whether this commit already writes the entity's projection.
    pub(super) fn projects_entity(&self) -> bool {
        self.projection.is_some()
    }

    /// Write the rows in `tx`, for the entity's journal at `sequence_nr`.
    pub(super) async fn write(
        &self,
        tx: &Transaction<'_>,
        [tenant, entity_type, entity_id]: [&str; 3],
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        match &self.projection {
            Some(Projection::Upsert {
                status,
                fields,
                state,
            }) => {
                let row = ProjectionWrite {
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    fields,
                    state,
                    sequence_nr,
                };
                write_projection(tx, &row).await?;
            }
            Some(Projection::Remove) => {
                remove_projection(tx, [tenant, entity_type, entity_id]).await?;
            }
            None => {}
        }
        match &self.state_timer {
            Some(TimerRow::Arm(timer)) => arm_timer(tx, timer).await?,
            Some(TimerRow::End) => {
                tx.execute(
                    "DELETE FROM state_timers \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            }
            None => {}
        }
        if let Some(vectors) = &self.vectors {
            tx.execute(
                // The entity's index: without it the planner may read every
                // vector row of the type (driver/plan_check.rs).
                "DELETE FROM entity_vector_index INDEXED BY idx_evi_entity \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
            for row in vectors {
                tx.execute(
                    "INSERT INTO entity_vector_index \
                     (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant,
                        entity_type,
                        row.decl_name.as_str(),
                        row.model_tag.as_str(),
                        entity_id,
                        Value::Blob(pack_f32_le(&row.vector)),
                        i64::try_from(sequence_nr).unwrap_or(i64::MAX),
                    ],
                )
                .await
                .map_err(storage_error)?;
            }
        }
        Ok(())
    }
}

/// The entity's timer row becomes `timer`. The append holds the entity's
/// newest sequence, so its row replaces whatever row there was.
async fn arm_timer(tx: &Transaction<'_>, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
    tx.execute(
        "INSERT INTO state_timers \
             (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
             state = excluded.state, \
             arm_id = excluded.arm_id, \
             sequence_nr = excluded.sequence_nr, \
             started_at = excluded.started_at, \
             params = excluded.params, \
             armed_by = excluded.armed_by",
        params![
            timer.tenant.as_str(),
            timer.entity_type.as_str(),
            timer.entity_id.as_str(),
            timer.state.as_str(),
            timer.arm_id.as_str(),
            i64::try_from(timer.sequence_nr).unwrap_or(i64::MAX),
            timer.started_at.as_str(),
            timer.params.as_str(),
            timer.armed_by.as_str(),
        ],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}
