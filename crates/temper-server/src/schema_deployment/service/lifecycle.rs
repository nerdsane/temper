use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(crate) async fn verify(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: VerifySchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        ensure_verification_available()?;
        let scope = parse_scope(&request.scope)?;
        self.authorize(
            tenant,
            security,
            "schema_bundle_verify",
            &scope,
            Some(&request.bundle_digest),
        )
        .await?;
        let now = simulated_millis()?;
        let lease = now.checked_add(120_000).ok_or_else(|| {
            ServiceError::new(
                "backend_unavailable",
                "logical verification lease exhausted",
                true,
            )
        })?;
        let operation = operation_identity(
            request.idempotency_key.clone(),
            request.request_id.clone(),
            &("verify", tenant, &scope, request.bundle_digest.as_str()),
        )?;
        let claim_outcome = self
            .store()?
            .claim_schema_verification(ClaimSchemaVerification {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                bundle_digest: request.bundle_digest.clone(),
                logical_now: now,
                lease_expires_at: lease,
                operation,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let claim = match claim_outcome {
            ClaimSchemaVerificationOutcome::Claimed(record) => {
                emit_schema_lifecycle(
                    tenant,
                    "SchemaDeployment",
                    &record.bundle.digest,
                    "verify_claim",
                    "submitted",
                    "verifying",
                    &scope,
                );
                record
            }
            ClaimSchemaVerificationOutcome::Replayed(record) => match record.status {
                SchemaDeploymentStatus::Verified => {
                    let mut response = receipt(&record);
                    response.request_id = record
                        .verification_request_id
                        .clone()
                        .unwrap_or_else(|| record.accepted_request_id.clone());
                    return Ok(response);
                }
                SchemaDeploymentStatus::Rejected => {
                    return Err(ServiceError::new(
                        "verification_failed",
                        "one or more required verification levels failed",
                        false,
                    ));
                }
                SchemaDeploymentStatus::Verifying => record,
                _ => {
                    return Err(ServiceError::new(
                        "invalid_lifecycle_transition",
                        "verification replay has no durable verifier result",
                        true,
                    ));
                }
            },
        };
        let passed = verify_bundle(self.state, &claim).await?;
        let input_digest = canonical_request_digest(&claim.bundle)?;
        let receipt_id = format!("verify:{}", &input_digest[7..23]);
        let record = self
            .store()?
            .finish_schema_verification(
                tenant,
                &scope,
                &request.bundle_digest,
                claim.fence,
                SchemaVerificationReceipt {
                    id: receipt_id,
                    verifier_version: "temper-verification-cascade/v1".into(),
                    input_digest,
                    passed,
                },
            )
            .await
            .map_err(ServiceError::from_store)?;
        emit_schema_lifecycle(
            tenant,
            "SchemaDeployment",
            &record.bundle.digest,
            "verify_finish",
            "verifying",
            if passed { "verified" } else { "rejected" },
            &scope,
        );
        if !passed {
            return Err(ServiceError::new(
                "verification_failed",
                "one or more required verification levels failed",
                false,
            ));
        }
        let mut response = receipt(&record);
        response.request_id = record
            .verification_request_id
            .clone()
            .unwrap_or_else(|| record.accepted_request_id.clone());
        Ok(response)
    }

    pub(crate) async fn activate(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: ActivateSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(
            tenant,
            security,
            "schema_bundle_activate",
            &scope,
            Some(&request.bundle_digest),
        )
        .await?;
        self.recover_registry_pointer(tenant, &scope).await?;
        let target = self
            .store()?
            .get_schema_deployment(tenant, &scope, &request.bundle_digest)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| {
                ServiceError::new("invalid_bundle", "schema deployment was not found", false)
            })?;
        self.stage_registry_bundle(&target)?;
        let operation = operation_identity(
            request.idempotency_key.clone(),
            request.request_id.clone(),
            &(
                "activate",
                tenant,
                &scope,
                request.bundle_digest.as_str(),
                request.expected_predecessor.as_deref(),
                request.expected_fence,
                request.verification_receipt_id.as_str(),
            ),
        )?;
        let outcome = self
            .store()?
            .activate_schema_bundle(ActivateSchemaBundle {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                bundle_digest: request.bundle_digest.clone(),
                expected_predecessor: request.expected_predecessor.clone(),
                expected_fence: request.expected_fence,
                verification_receipt_id: request.verification_receipt_id.clone(),
                operation,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let (pointer, replayed) = match outcome {
            ActivateSchemaBundleOutcome::Activated(pointer) => (pointer, false),
            ActivateSchemaBundleOutcome::Replayed(pointer) => (pointer, true),
        };
        if !replayed {
            emit_schema_lifecycle(
                tenant,
                "SchemaDeployment",
                &pointer.bundle_digest,
                "activate",
                "verified",
                "active",
                &scope,
            );
        }
        let mut registry = self.state.registry.write().map_err(|_| {
            ServiceError::new(
                "backend_unavailable",
                "spec registry lock is unavailable",
                true,
            )
        })?;
        if !replayed {
            registry
                .activate_scoped_bundle(
                    &TenantId::new(tenant),
                    &scope,
                    &request.bundle_digest,
                    request.expected_predecessor.as_deref(),
                )
                .map_err(|error| {
                    ServiceError::new("backend_unavailable", error.to_string(), true)
                })?;
        }
        Ok(SchemaDeploymentReceiptV1 {
            request_id: pointer.accepted_request_id,
            scope: scope_v1(&scope),
            bundle_digest: pointer.bundle_digest,
            predecessor: pointer.predecessor_digest,
            status: "active".into(),
            fence: pointer.fence,
            verification_receipt_id: Some(request.verification_receipt_id),
            migration_receipt_id: None,
            committed_sequence: pointer.committed_sequence,
        })
    }

    pub(crate) async fn retire(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: RetireSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(
            tenant,
            security,
            "schema_bundle_retire",
            &scope,
            Some(&request.bundle_digest),
        )
        .await?;
        self.recover_registry_pointer(tenant, &scope).await?;
        let operation = operation_identity(
            request.idempotency_key.clone(),
            request.request_id.clone(),
            &(
                "retire",
                tenant,
                &scope,
                request.bundle_digest.as_str(),
                request.expected_fence,
            ),
        )?;
        let outcome = self
            .store()?
            .retire_schema_bundle(RetireSchemaBundle {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                bundle_digest: request.bundle_digest.clone(),
                expected_fence: request.expected_fence,
                operation,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let (record, retired) = match outcome {
            RetireSchemaBundleOutcome::Retired(record) => (record, true),
            RetireSchemaBundleOutcome::Replayed(record) => (record, false),
        };
        if retired {
            emit_schema_lifecycle(
                tenant,
                "SchemaDeployment",
                &record.bundle.digest,
                "retire",
                "active",
                "retired",
                &scope,
            );
        }
        let mut registry = self.state.registry.write().map_err(|_| {
            ServiceError::new(
                "backend_unavailable",
                "spec registry lock is unavailable",
                true,
            )
        })?;
        if registry.active_scope_digest(&TenantId::new(tenant), &scope)
            == Some(request.bundle_digest.as_str())
        {
            registry
                .retire_scoped_bundle(&TenantId::new(tenant), &scope, &request.bundle_digest)
                .map_err(|error| {
                    ServiceError::new("backend_unavailable", error.to_string(), true)
                })?;
        }
        let mut response = receipt(&record);
        response.request_id = record
            .retirement_request_id
            .clone()
            .unwrap_or_else(|| record.accepted_request_id.clone());
        Ok(response)
    }
}
