//! Durable job state and bounded receipt helpers.

use super::*;
pub(super) const JOB_EVENT: &str = "_TemperGovernedStreamDescriptorMigrationV1";
pub(super) const JOB_EVENT_BUDGET: usize = 1_024;
pub(super) const JOB_PAYLOAD_BYTE_BUDGET: usize = 4 * 1024 * 1024;
pub(super) const MAX_PAGE_SUBJECTS: u32 = 256;
pub(super) const MAX_EVENTS_PER_SUBJECT: u32 = 1_024;
pub(super) const MAX_PAGE_BLOB_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(super) const MAX_UNRESOLVED_PAGE: u32 = 256;
pub(super) const STAGED_APPLICATION_EVENT: &str = "_TemperStagedApplicationStreamCapabilitiesV1";
const OPERATION_ID_BYTE_BUDGET: usize = 256;

pub(super) fn validate_operation_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > OPERATION_ID_BYTE_BUDGET
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} is not a bounded canonical identifier"));
    }
    Ok(())
}

pub(super) fn validate_job_id(value: &str) -> Result<(), String> {
    let digest = value
        .strip_prefix("sdm:")
        .ok_or_else(|| "stream descriptor migration job id is invalid".to_string())?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("stream descriptor migration job id is invalid".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StagedInstalledApplicationV1 {
    pub(super) application_id: String,
    pub(super) semantic_digest: String,
    pub(super) capabilities: Vec<VerifiedStreamCapabilityV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct DurableJobV1 {
    pub(super) contract_version: u16,
    pub(super) request_digest: String,
    pub(super) idempotency_key: String,
    pub(super) accepted_request_id: String,
    pub(super) job_id: String,
    pub(super) target: StreamDescriptorMigrationTargetV1,
    pub(super) capability_digest: String,
    pub(super) descriptor_contract_version: u16,
    pub(super) budgets: StreamDescriptorMigrationBudgetsV1,
    pub(super) capabilities: Vec<VerifiedStreamCapabilityV1>,
    pub(super) source_bundle_digest: Option<String>,
    pub(super) capability_index: usize,
    pub(super) after_entity_id: Option<String>,
    pub(super) scan_complete: bool,
    pub(super) scanned_subjects: u64,
    pub(super) migrated_subjects: u64,
    pub(super) unresolved: BTreeMap<String, String>,
    pub(super) resolved: BTreeSet<(String, String)>,
    pub(super) latest_page_outcomes: Vec<StreamDescriptorMigrationPageOutcomeV1>,
    pub(super) retry_after: Option<(String, String)>,
    pub(super) scan_generation: PublicationGenerationV1,
    pub(super) completion_generation: Option<PublicationGenerationV1>,
    pub(super) completion_receipt_id: Option<String>,
    pub(super) start_receipt: Option<StreamDescriptorMigrationReceiptV1>,
    pub(super) advance_operations: BTreeMap<String, DurableAdvanceReplayV1>,
    pub(super) committed_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicationGenerationV1 {
    pub(super) token: String,
    pub(super) scoped_write_version: Option<u64>,
    pub(super) unscoped_write_versions: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct DurableAdvanceReplayV1 {
    pub(super) request_digest: String,
    pub(super) receipt: StreamDescriptorMigrationReceiptV1,
}

pub(super) fn validate_budgets(budgets: &StreamDescriptorMigrationBudgetsV1) -> Result<(), String> {
    if budgets.max_subjects == 0
        || budgets.max_subjects > MAX_PAGE_SUBJECTS
        || budgets.max_events_per_subject == 0
        || budgets.max_events_per_subject > MAX_EVENTS_PER_SUBJECT
        || budgets.max_blob_bytes == 0
        || budgets.max_blob_bytes > MAX_PAGE_BLOB_BYTES
    {
        return Err("stream descriptor migration budgets are outside supported bounds".into());
    }
    Ok(())
}

pub(super) fn job_receipt(
    job: &DurableJobV1,
    request_id: String,
) -> StreamDescriptorMigrationReceiptV1 {
    job_receipt_at(job, request_id, job.committed_sequence)
}

pub(super) fn job_receipt_at(
    job: &DurableJobV1,
    request_id: String,
    committed_sequence: u64,
) -> StreamDescriptorMigrationReceiptV1 {
    StreamDescriptorMigrationReceiptV1 {
        request_id,
        job_id: job.job_id.clone(),
        target: job.target.clone(),
        capability_digest: job.capability_digest.clone(),
        descriptor_contract_version: job.descriptor_contract_version,
        status: if job.completion_receipt_id.is_some() {
            "completed"
        } else if job.scan_complete {
            "unresolved"
        } else {
            "migrating"
        }
        .into(),
        cursor: (!job.scan_complete).then(|| {
            let mut cursor_hasher = Sha256::new();
            cursor_hasher.update(job.capability_index.to_be_bytes());
            if let Some(entity_id) = job.after_entity_id.as_deref() {
                cursor_hasher.update([1]);
                cursor_hasher.update(entity_id.len().to_be_bytes());
                cursor_hasher.update(entity_id.as_bytes());
            } else {
                cursor_hasher.update([0]);
            }
            format!(
                "cursor:{}:{:x}",
                committed_sequence,
                cursor_hasher.finalize()
            )
        }),
        scanned_subjects: job.scanned_subjects,
        migrated_subjects: job.migrated_subjects,
        unresolved_subjects: job.unresolved.len() as u64,
        page_outcomes: job.latest_page_outcomes.clone(),
        completion_receipt_id: job.completion_receipt_id.clone(),
        committed_sequence,
    }
}

pub(super) fn completion_id(tenant: &TenantId, job: &DurableJobV1) -> Result<String, String> {
    let evidence = (
        tenant.as_str(),
        &job.target,
        &job.capability_digest,
        job.descriptor_contract_version,
        &job.source_bundle_digest,
        job.scanned_subjects,
        job.migrated_subjects,
        &job.completion_generation,
    );
    Ok(format!(
        "sdm-complete:{:x}",
        Sha256::digest(serde_json::to_vec(&evidence).map_err(|error| error.to_string())?)
    ))
}

pub(super) fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

pub(super) fn parse_unresolved_cursor(value: &str) -> Result<usize, String> {
    value
        .strip_prefix("unresolved:")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "invalid unresolved cursor".into())
}

pub(super) fn unresolved_subject_key(entity_type: &str, entity_id: &str) -> Result<String, String> {
    serde_json::to_string(&(entity_type, entity_id)).map_err(|error| error.to_string())
}

pub(super) fn parse_unresolved_subject_key(value: &str) -> Result<(String, String), String> {
    serde_json::from_str(value)
        .map_err(|error| format!("stream descriptor unresolved subject key is invalid: {error}"))
}

pub(super) fn job_persistence_id(tenant: &TenantId, job_id: &str) -> String {
    format!("{tenant}:_TemperStreamMigration:{job_id}")
}
pub(super) fn staged_application_persistence_id(tenant: &TenantId, application_id: &str) -> String {
    format!(
        "{tenant}:_TemperStreamMigrationTarget:{:x}",
        Sha256::digest(application_id.as_bytes())
    )
}
pub(super) fn local_type(value: &str) -> &str {
    value.rsplit('.').next().unwrap_or(value)
}
pub(super) fn target_kind(target: &StreamDescriptorMigrationTargetV1) -> &'static str {
    match target {
        StreamDescriptorMigrationTargetV1::TaskBundle { .. } => "task_bundle",
        StreamDescriptorMigrationTargetV1::InstalledApplication { .. } => "installed_application",
    }
}
