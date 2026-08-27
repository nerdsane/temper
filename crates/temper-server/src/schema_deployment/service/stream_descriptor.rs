use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(crate) async fn start_stream_descriptor_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: StartStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, ServiceError> {
        self.authorize_stream_target(
            tenant,
            security,
            "stream_descriptor_migration_start",
            &request.target,
        )
        .await?;
        self.state
            .start_governed_stream_descriptor_migration_v1(&TenantId::new(tenant), request)
            .await
            .map_err(stream_error)
    }

    pub(crate) async fn advance_stream_descriptor_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: AdvanceStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, ServiceError> {
        let target = job_target(self.state, tenant, &request.job_id).await?;
        self.authorize_stream_target(
            tenant,
            security,
            "stream_descriptor_migration_advance",
            &target,
        )
        .await?;
        self.state
            .advance_governed_stream_descriptor_migration_v1(&TenantId::new(tenant), request)
            .await
            .map_err(stream_error)
    }

    pub(crate) async fn get_stream_descriptor_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: GetStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, ServiceError> {
        let target = job_target(self.state, tenant, &request.job_id).await?;
        self.authorize_stream_target(tenant, security, "stream_descriptor_migration_get", &target)
            .await?;
        self.state
            .get_governed_stream_descriptor_migration_v1(&TenantId::new(tenant), request)
            .await
            .map_err(stream_error)
    }

    pub(crate) async fn list_unresolved_stream_descriptors(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: ListUnresolvedStreamDescriptorsRequestV1,
    ) -> Result<UnresolvedStreamDescriptorPageV1, ServiceError> {
        let target = job_target(self.state, tenant, &request.job_id).await?;
        self.authorize_stream_target(
            tenant,
            security,
            "stream_descriptor_migration_list_unresolved",
            &target,
        )
        .await?;
        self.state
            .list_governed_unresolved_stream_descriptors_v1(&TenantId::new(tenant), request)
            .await
            .map_err(stream_error)
    }

    async fn authorize_stream_target(
        &self,
        tenant: &str,
        security: &SecurityContext,
        action: &str,
        target: &StreamDescriptorMigrationTargetV1,
    ) -> Result<(), ServiceError> {
        match target {
            StreamDescriptorMigrationTargetV1::TaskBundle {
                scope,
                bundle_digest,
            } => {
                let scope = parse_scope(scope)?;
                self.authorize(tenant, security, action, &scope, Some(bundle_digest))
                    .await
            }
            StreamDescriptorMigrationTargetV1::InstalledApplication {
                application_id,
                semantic_digest,
            } => {
                if application_id.is_empty() || semantic_digest.is_empty() {
                    return Err(ServiceError::new(
                        "invalid_bundle",
                        "installed application target is invalid",
                        false,
                    ));
                }
                self.authorize_installed_application_stream_migration(
                    tenant,
                    security,
                    action,
                    application_id,
                    Some(semantic_digest),
                )
                .await
            }
        }
    }
}

async fn job_target(
    state: &ServerState,
    tenant: &str,
    job_id: &str,
) -> Result<StreamDescriptorMigrationTargetV1, ServiceError> {
    let receipt = state
        .get_governed_stream_descriptor_migration_v1(
            &TenantId::new(tenant),
            GetStreamDescriptorMigrationRequestV1 {
                request_id: "authorization".into(),
                job_id: job_id.into(),
            },
        )
        .await
        .map_err(stream_error)?;
    Ok(receipt.target)
}

pub(super) fn stream_error(message: String) -> ServiceError {
    let (code, retryable) = if message.starts_with("backend unavailable:") {
        ("backend_unavailable", true)
    } else if message.starts_with("stale fence:") {
        ("stale_fence", true)
    } else if message.contains("bounded canonical identifier")
        || message.contains("job id is invalid")
        || message.contains("budgets are outside supported bounds")
        || message.contains("unsupported stream descriptor contract version")
        || message.contains("not found")
    {
        ("invalid_bundle", false)
    } else if message.contains("evidence") || message.contains("unresolved") {
        ("migration_required", false)
    } else if message.contains("idempotency") {
        ("idempotency_conflict", false)
    } else {
        ("migration_rejected", false)
    };
    ServiceError::new(code, message, retryable)
}

pub(super) fn stream_descriptor_http_response(
    result: Result<StreamDescriptorMigrationReceiptV1, ServiceError>,
) -> Response {
    match result {
        Ok(receipt) => (
            StatusCode::OK,
            axum::Json(SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt }),
        )
            .into_response(),
        Err(error) => (
            error.status(),
            axum::Json(SchemaDeploymentResponseV1::Error {
                error: error.response,
            }),
        )
            .into_response(),
    }
}

pub(super) fn unresolved_stream_descriptor_http_response(
    result: Result<UnresolvedStreamDescriptorPageV1, ServiceError>,
) -> Response {
    match result {
        Ok(page) => (
            StatusCode::OK,
            axum::Json(SchemaDeploymentResponseV1::UnresolvedStreamDescriptors { page }),
        )
            .into_response(),
        Err(error) => (
            error.status(),
            axum::Json(SchemaDeploymentResponseV1::Error {
                error: error.response,
            }),
        )
            .into_response(),
    }
}
