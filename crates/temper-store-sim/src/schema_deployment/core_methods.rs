macro_rules! impl_schema_core_methods {
    () => {
        async fn submit_schema_bundle(
            &self,
            command: SubmitSchemaBundle,
        ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError> {
            validate_text("tenant", &command.bundle.tenant)?;
            validate_text("scope id", &command.bundle.scope.id)?;
            validate_text("bundle digest", &command.bundle.digest)?;
            validate_digest("bundle digest", &command.bundle.digest)?;
            if let Some(predecessor) = command.bundle.predecessor_digest.as_deref() {
                validate_digest("predecessor digest", predecessor)?;
            }
            for digest in command.bundle.wasm_module_digests.values() {
                validate_digest("WASM module digest", digest)?;
            }
            if let Some(digest) = command.bundle.migration_module_digest.as_deref() {
                validate_digest("migration module digest", digest)?;
            }
            validate_text("idempotency key", &command.idempotency_key)?;
            validate_text("request digest", &command.request_digest)?;
            validate_digest("request digest", &command.request_digest)?;
            validate_text("request id", &command.request_id)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::SubmitBundle)?;
            let idempotency_key = (
                command.bundle.tenant.clone(),
                "submit".to_string(),
                command.idempotency_key.clone(),
            );
            if let Some((request_digest, bundle_digest, _)) =
                inner.schema_deployments.idempotency.get(&idempotency_key)
            {
                if request_digest != &command.request_digest {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let key =
                    deployment_key(&command.bundle.tenant, &command.bundle.scope, bundle_digest);
                let deployment = inner
                    .schema_deployments
                    .deployments
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| {
                        SchemaDeploymentStoreError::BackendUnavailable(
                            "idempotency record lost its deployment".into(),
                        )
                    })?;
                return Ok(SubmitSchemaBundleOutcome::Replayed(deployment));
            }

            let key = deployment_key(
                &command.bundle.tenant,
                &command.bundle.scope,
                &command.bundle.digest,
            );
            if let Some(existing) = inner.schema_deployments.deployments.get(&key).cloned() {
                if existing.bundle != command.bundle {
                    return Err(SchemaDeploymentStoreError::InvalidInput(
                        "bundle digest aliases different canonical artifacts".into(),
                    ));
                }
                inner.schema_deployments.idempotency.insert(
                    idempotency_key,
                    (command.request_digest, command.bundle.digest, None),
                );
                return Ok(SubmitSchemaBundleOutcome::Replayed(existing));
            }

            let deployment = SchemaDeploymentRecord {
                bundle: command.bundle.clone(),
                status: SchemaDeploymentStatus::Submitted,
                fence: 0,
                lease_expires_at: None,
                verification_receipt_id: None,
                verification_replay: None,
                activation_pointer: None,
                committed_sequence: 1,
                accepted_request_id: command.request_id,
                verification_request_id: None,
                retirement_request_id: None,
            };
            inner
                .schema_deployments
                .deployments
                .insert(key, deployment.clone());
            inner.schema_deployments.idempotency.insert(
                idempotency_key,
                (command.request_digest, command.bundle.digest, None),
            );
            Ok(SubmitSchemaBundleOutcome::Created(deployment))
        }

        async fn get_schema_deployment(
            &self,
            tenant: &str,
            scope: &SchemaScope,
            digest: &str,
        ) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .deployments
                .get(&deployment_key(tenant, scope, digest))
                .cloned())
        }

        async fn claim_schema_verification(
            &self,
            command: ClaimSchemaVerification,
        ) -> Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError> {
            validate_operation(
                &command.tenant,
                &command.scope,
                &command.bundle_digest,
                &command.operation,
            )?;
            if command.lease_expires_at <= command.logical_now {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "verification lease must end after logical now".into(),
                ));
            }
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ClaimVerification)?;
            let idempotency_key = (
                command.tenant.clone(),
                "verify".to_string(),
                command.operation.idempotency_key.clone(),
            );
            if let Some((request_digest, bundle_digest, _)) =
                inner.schema_deployments.idempotency.get(&idempotency_key)
            {
                if request_digest != &command.operation.request_digest
                    || bundle_digest != &command.bundle_digest
                {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let record = inner
                    .schema_deployments
                    .deployments
                    .get(&deployment_key(
                        &command.tenant,
                        &command.scope,
                        &command.bundle_digest,
                    ))
                    .cloned()
                    .ok_or_else(|| {
                        SchemaDeploymentStoreError::BackendUnavailable(
                            "verification idempotency record lost its deployment".into(),
                        )
                    })?;
                let replay = record.verification_replay_record().unwrap_or(record);
                return Ok(ClaimSchemaVerificationOutcome::Replayed(replay));
            }
            let deployment = inner
                .schema_deployments
                .deployments
                .get_mut(&deployment_key(
                    &command.tenant,
                    &command.scope,
                    &command.bundle_digest,
                ))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            let claimable = deployment.status == SchemaDeploymentStatus::Submitted
                || (deployment.status == SchemaDeploymentStatus::Verifying
                    && deployment
                        .lease_expires_at
                        .is_some_and(|deadline| deadline <= command.logical_now));
            if !claimable {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            let next_fence = checked_next(deployment.fence, "verification fence")?;
            let next_sequence = checked_next(deployment.committed_sequence, "deployment sequence")?;
            deployment.status = SchemaDeploymentStatus::Verifying;
            deployment.verification_request_id = Some(command.operation.request_id.clone());
            deployment.fence = next_fence;
            deployment.committed_sequence = next_sequence;
            deployment.lease_expires_at = Some(command.lease_expires_at);
            let result = deployment.clone();
            inner.schema_deployments.idempotency.insert(
                idempotency_key,
                (
                    command.operation.request_digest,
                    command.bundle_digest,
                    None,
                ),
            );
            Ok(ClaimSchemaVerificationOutcome::Claimed(result))
        }

        async fn finish_schema_verification(
            &self,
            tenant: &str,
            scope: &SchemaScope,
            digest: &str,
            expected_fence: u64,
            receipt: SchemaVerificationReceipt,
        ) -> Result<SchemaDeploymentRecord, SchemaDeploymentStoreError> {
            validate_text("verification receipt id", &receipt.id)?;
            validate_digest("verification input digest", &receipt.input_digest)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::FinishVerification)?;
            let key = deployment_key(tenant, scope, digest);
            let receipt_key = (
                tenant.to_string(),
                scope.clone(),
                digest.to_string(),
                receipt.id.clone(),
            );
            if let Some(existing) = inner
                .schema_deployments
                .verification_receipts
                .get(&receipt_key)
                && existing != &receipt
            {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "verification receipt identity conflict".into(),
                ));
            }
            let deployment = inner
                .schema_deployments
                .deployments
                .get_mut(&key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if deployment.status != SchemaDeploymentStatus::Verifying {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if deployment.fence != expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let next_sequence = checked_next(deployment.committed_sequence, "deployment sequence")?;
            deployment.status = if receipt.passed {
                SchemaDeploymentStatus::Verified
            } else {
                SchemaDeploymentStatus::Rejected
            };
            deployment.lease_expires_at = None;
            deployment.verification_receipt_id = Some(receipt.id.clone());
            deployment.committed_sequence = next_sequence;
            deployment.verification_replay = Some(SchemaVerificationReplay {
                status: deployment.status,
                fence: deployment.fence,
                committed_sequence: deployment.committed_sequence,
                verification_receipt_id: receipt.id.clone(),
            });
            let result = deployment.clone();
            inner
                .schema_deployments
                .verification_receipts
                .insert(receipt_key, receipt);
            Ok(result)
        }

        async fn activate_schema_bundle(
            &self,
            command: ActivateSchemaBundle,
        ) -> Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError> {
            validate_operation(
                &command.tenant,
                &command.scope,
                &command.bundle_digest,
                &command.operation,
            )?;
            if let Some(predecessor) = command.expected_predecessor.as_deref() {
                validate_digest("expected predecessor digest", predecessor)?;
            }
            let tenant = command.tenant.as_str();
            let scope = &command.scope;
            let digest = command.bundle_digest.as_str();
            let expected_predecessor = command.expected_predecessor.as_deref();
            let verification_receipt_id = command.verification_receipt_id.as_str();
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ActivateBundle)?;
            let idempotency_key = (
                command.tenant.clone(),
                "activate".to_string(),
                command.operation.idempotency_key.clone(),
            );
            if let Some((request_digest, bundle_digest, response)) =
                inner.schema_deployments.idempotency.get(&idempotency_key)
            {
                if request_digest != &command.operation.request_digest
                    || bundle_digest != &command.bundle_digest
                {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let pointer = serde_json::from_str(response.as_deref().ok_or_else(|| {
                    SchemaDeploymentStoreError::BackendUnavailable(
                        "activation idempotency record lost its receipt".into(),
                    )
                })?)
                .map_err(|error| {
                    SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
                })?;
                return Ok(ActivateSchemaBundleOutcome::Replayed(pointer));
            }
            let scope_key = (tenant.to_string(), scope.clone());
            let active_digest = inner
                .schema_deployments
                .active
                .get(&scope_key)
                .map(|pointer| pointer.bundle_digest.as_str());
            if active_digest != expected_predecessor {
                return Err(SchemaDeploymentStoreError::PredecessorMismatch);
            }
            let key = deployment_key(tenant, scope, digest);
            let deployment = inner
                .schema_deployments
                .deployments
                .get(&key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if deployment.status != SchemaDeploymentStatus::Verified {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if deployment.fence != command.expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            if deployment.bundle.predecessor_digest.as_deref() != expected_predecessor {
                return Err(SchemaDeploymentStoreError::PredecessorMismatch);
            }
            if deployment.verification_receipt_id.as_deref() != Some(verification_receipt_id) {
                return Err(SchemaDeploymentStoreError::VerificationFailed);
            }
            let receipt_key = (
                tenant.to_string(),
                scope.clone(),
                digest.to_string(),
                verification_receipt_id.to_string(),
            );
            if !inner
                .schema_deployments
                .verification_receipts
                .get(&receipt_key)
                .is_some_and(|receipt| receipt.passed)
            {
                return Err(SchemaDeploymentStoreError::VerificationFailed);
            }

            let next_target_fence = checked_next(deployment.fence, "activation fence")?;
            let next_target_sequence =
                checked_next(deployment.committed_sequence, "deployment sequence")?;
            let next_predecessor_sequence = expected_predecessor
                .and_then(|predecessor| {
                    inner.schema_deployments.deployments.get(&deployment_key(
                        tenant,
                        scope,
                        predecessor,
                    ))
                })
                .map(|previous| checked_next(previous.committed_sequence, "deployment sequence"))
                .transpose()?;

            let deployment = inner
                .schema_deployments
                .deployments
                .get_mut(&key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            deployment.status = SchemaDeploymentStatus::Active;
            deployment.fence = next_target_fence;
            deployment.committed_sequence = next_target_sequence;
            let pointer = SchemaActivePointer {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                bundle_digest: digest.to_string(),
                predecessor_digest: expected_predecessor.map(str::to_string),
                fence: deployment.fence,
                committed_sequence: deployment.committed_sequence,
                accepted_request_id: command.operation.request_id.clone(),
            };
            deployment.activation_pointer = Some(pointer.clone());
            let encoded_pointer = serde_json::to_string(&pointer).map_err(|error| {
                SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
            })?;
            inner
                .schema_deployments
                .active
                .insert(scope_key, pointer.clone());
            if let (Some(predecessor), Some(next_sequence)) =
                (expected_predecessor, next_predecessor_sequence)
            {
                let predecessor_key = deployment_key(tenant, scope, predecessor);
                if let Some(previous) = inner
                    .schema_deployments
                    .deployments
                    .get_mut(&predecessor_key)
                {
                    previous.status = SchemaDeploymentStatus::Retired;
                    previous.committed_sequence = next_sequence;
                }
            }
            inner.schema_deployments.idempotency.insert(
                idempotency_key,
                (
                    command.operation.request_digest,
                    command.bundle_digest,
                    Some(encoded_pointer),
                ),
            );
            Ok(ActivateSchemaBundleOutcome::Activated(pointer))
        }

        impl_schema_pointer_method!();

        async fn retire_schema_bundle(
            &self,
            command: RetireSchemaBundle,
        ) -> Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError> {
            validate_operation(
                &command.tenant,
                &command.scope,
                &command.bundle_digest,
                &command.operation,
            )?;
            let tenant = command.tenant.as_str();
            let scope = &command.scope;
            let digest = command.bundle_digest.as_str();
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::RetireBundle)?;
            let idempotency_key = (
                command.tenant.clone(),
                "retire".to_string(),
                command.operation.idempotency_key.clone(),
            );
            if let Some((request_digest, bundle_digest, response)) =
                inner.schema_deployments.idempotency.get(&idempotency_key)
            {
                if request_digest != &command.operation.request_digest
                    || bundle_digest != &command.bundle_digest
                {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let record = serde_json::from_str(response.as_deref().ok_or_else(|| {
                    SchemaDeploymentStoreError::BackendUnavailable(
                        "retirement idempotency record lost its receipt".into(),
                    )
                })?)
                .map_err(|error| {
                    SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
                })?;
                return Ok(RetireSchemaBundleOutcome::Replayed(record));
            }
            let scope_key = (tenant.to_string(), scope.clone());
            let pointer = inner
                .schema_deployments
                .active
                .get(&scope_key)
                .ok_or(SchemaDeploymentStoreError::InvalidLifecycleTransition)?;
            if pointer.bundle_digest != digest {
                return Err(SchemaDeploymentStoreError::PredecessorMismatch);
            }
            if pointer.fence != command.expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let record = inner
                .schema_deployments
                .deployments
                .get_mut(&deployment_key(tenant, scope, digest))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if record.status != SchemaDeploymentStatus::Active
                || record.fence != command.expected_fence
            {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let next_fence = checked_next(record.fence, "retirement fence")?;
            let next_sequence = checked_next(record.committed_sequence, "deployment sequence")?;
            record.status = SchemaDeploymentStatus::Retired;
            record.retirement_request_id = Some(command.operation.request_id.clone());
            record.fence = next_fence;
            record.committed_sequence = next_sequence;
            let result = record.clone();
            let encoded_result = serde_json::to_string(&result).map_err(|error| {
                SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
            })?;
            inner.schema_deployments.active.remove(&scope_key);
            inner.schema_deployments.idempotency.insert(
                idempotency_key,
                (
                    command.operation.request_digest,
                    command.bundle_digest,
                    Some(encoded_result),
                ),
            );
            Ok(RetireSchemaBundleOutcome::Retired(result))
        }
    };
}
