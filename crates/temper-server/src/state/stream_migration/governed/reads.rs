//! Read-only governed migration progress operations.

use super::*;

impl ServerState {
    /// Read durable migration progress.
    pub(crate) async fn get_governed_stream_descriptor_migration_v1(
        &self,
        tenant: &TenantId,
        request: GetStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, String> {
        validate_operation_id("request id", &request.request_id)?;
        validate_job_id(&request.job_id)?;
        let job = self
            .load_governed_job(tenant, &request.job_id)
            .await?
            .ok_or_else(|| "stream descriptor migration job was not found".to_string())?;
        Ok(job_receipt(&job, request.request_id))
    }

    /// Read redacted unresolved classifications with a bounded opaque cursor.
    pub(crate) async fn list_governed_unresolved_stream_descriptors_v1(
        &self,
        tenant: &TenantId,
        request: ListUnresolvedStreamDescriptorsRequestV1,
    ) -> Result<UnresolvedStreamDescriptorPageV1, String> {
        validate_operation_id("request id", &request.request_id)?;
        validate_job_id(&request.job_id)?;
        if request.limit == 0 || request.limit > MAX_UNRESOLVED_PAGE {
            return Err("unresolved page limit is outside the supported budget".into());
        }
        let job = self
            .load_governed_job(tenant, &request.job_id)
            .await?
            .ok_or_else(|| "stream descriptor migration job was not found".to_string())?;
        let offset = request
            .after
            .as_deref()
            .map(parse_unresolved_cursor)
            .transpose()?
            .unwrap_or(0);
        let limit = usize::try_from(request.limit).map_err(|_| "invalid page limit")?;
        let entries = job
            .unresolved
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(key, classification)| {
                let (entity_type, entity_id) = parse_unresolved_subject_key(key)?;
                Ok(UnresolvedStreamDescriptorV1 {
                    subject_digest: format!(
                        "sha256:{:x}",
                        Sha256::digest(
                            [entity_type.as_bytes(), b"\0", entity_id.as_bytes()].concat()
                        )
                    ),
                    classification: classification.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let next_offset = offset.saturating_add(entries.len());
        let next =
            (next_offset < job.unresolved.len()).then(|| format!("unresolved:{next_offset}"));
        Ok(UnresolvedStreamDescriptorPageV1 {
            request_id: request.request_id,
            job_id: request.job_id,
            entries,
            next,
        })
    }
}
