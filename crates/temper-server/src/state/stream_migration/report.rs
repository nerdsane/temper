//! Durable, idempotent migration inventory-page reports.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::{ServerState, StreamDescriptorBackfillCandidateV1, StreamDescriptorBackfillOutcomeV1};

pub(super) const MIGRATION_CURSOR_BYTE_BUDGET: usize = 512;
const STREAM_DESCRIPTOR_MIGRATION_PAGE_EVENT: &str = "_TemperStreamDescriptorMigrationPageV1";
const MIGRATION_REPORT_EVENT_BUDGET: usize = 1_024;

/// Durable receipt for one idempotent migration inventory page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptorMigrationPageReceiptV1 {
    /// Caller-owned resumable inventory cursor.
    pub cursor: String,
    /// Durable migration-report journal sequence.
    pub report_sequence: u64,
    /// Outcomes in the same order as the bounded candidates.
    pub outcomes: Vec<StreamDescriptorBackfillOutcomeV1>,
    /// True only for an explicitly final page with no unresolved records.
    pub migration_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DurableStreamDescriptorMigrationPageV1 {
    pub(super) contract_version: u16,
    pub(super) cursor: String,
    pub(super) final_page: bool,
    pub(super) candidates: Vec<StreamDescriptorBackfillCandidateV1>,
    pub(super) outcomes: Vec<StreamDescriptorBackfillOutcomeV1>,
    pub(super) migration_complete: bool,
}

impl ServerState {
    pub(super) async fn persist_stream_descriptor_migration_page(
        &self,
        tenant: &TenantId,
        mut page: DurableStreamDescriptorMigrationPageV1,
    ) -> Result<StreamDescriptorMigrationPageReceiptV1, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "event journal is unavailable".to_string())?;
        let persistence_id = format!("{tenant}:_TemperStreamMigration:stream-descriptor-v1");
        let prior = journal
            .read_latest_events(
                &persistence_id,
                MIGRATION_REPORT_EVENT_BUDGET.saturating_add(1),
            )
            .await
            .map_err(|error| error.to_string())?;
        if prior.len() > MIGRATION_REPORT_EVENT_BUDGET {
            return Err("stream descriptor migration report exceeds its event budget".into());
        }
        let mut unresolved = BTreeSet::new();
        for existing in &prior {
            if existing.event_type != STREAM_DESCRIPTOR_MIGRATION_PAGE_EVENT {
                return Err("durable migration report contains an unexpected event".into());
            }
            let existing_page: DurableStreamDescriptorMigrationPageV1 =
                serde_json::from_value(existing.payload.clone())
                    .map_err(|error| format!("durable migration report is invalid: {error}"))?;
            if existing_page.cursor == page.cursor {
                if existing_page.candidates != page.candidates
                    || existing_page.final_page != page.final_page
                {
                    return Err("migration cursor was reused for different inventory input".into());
                }
                return Ok(StreamDescriptorMigrationPageReceiptV1 {
                    cursor: existing_page.cursor,
                    report_sequence: existing.sequence_nr,
                    outcomes: existing_page.outcomes,
                    migration_complete: existing_page.migration_complete,
                });
            }
            apply_page_outcomes(&mut unresolved, &existing_page)?;
        }
        apply_page_outcomes(&mut unresolved, &page)?;
        page.migration_complete = page.final_page && unresolved.is_empty();
        let expected_sequence = prior.last().map_or(0, |event| event.sequence_nr);
        let report_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| "stream descriptor migration report sequence overflowed".to_string())?;
        let envelope = PersistenceEnvelope {
            sequence_nr: report_sequence,
            event_type: STREAM_DESCRIPTOR_MIGRATION_PAGE_EVENT.into(),
            payload: serde_json::to_value(&page).map_err(|error| error.to_string())?,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
                kernel: None,
            },
        };
        journal
            .append(&persistence_id, expected_sequence, &[envelope])
            .await
            .map_err(|error| error.to_string())?;
        Ok(StreamDescriptorMigrationPageReceiptV1 {
            cursor: page.cursor,
            report_sequence,
            outcomes: page.outcomes,
            migration_complete: page.migration_complete,
        })
    }
}

fn apply_page_outcomes(
    unresolved: &mut BTreeSet<(String, String)>,
    page: &DurableStreamDescriptorMigrationPageV1,
) -> Result<(), String> {
    if page.contract_version != 1 || page.candidates.len() != page.outcomes.len() {
        return Err("durable migration report has an invalid contract shape".into());
    }
    for (candidate, outcome) in page.candidates.iter().zip(&page.outcomes) {
        let identity = (candidate.entity_type.clone(), candidate.entity_id.clone());
        match outcome {
            StreamDescriptorBackfillOutcomeV1::Unresolved { .. } => {
                unresolved.insert(identity);
            }
            StreamDescriptorBackfillOutcomeV1::Appended { .. }
            | StreamDescriptorBackfillOutcomeV1::AlreadyPresent { .. } => {
                unresolved.remove(&identity);
            }
        }
    }
    Ok(())
}
