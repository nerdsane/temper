macro_rules! impl_schema_bootstrap_methods {
    () => {
        async fn reserve_schema_bootstrap(
            &self,
            command: ReserveSchemaBootstrap,
        ) -> Result<ReserveSchemaBootstrapOutcome, SchemaDeploymentStoreError> {
            validate_schema_bootstrap_reservation(&command)
                .map_err(SchemaDeploymentStoreError::InvalidInput)?;
            validate_text("tenant", &command.tenant)?;
            validate_text("caller authority", &command.caller_authority)?;
            validate_digest("caller authority", &command.caller_authority)?;
            validate_text("accepted authority", &command.accepted_authority_json)?;
            validate_text("idempotency key", &command.idempotency_key)?;
            validate_text("request digest", &command.request_digest)?;
            validate_digest("request digest", &command.request_digest)?;
            validate_text("request id", &command.request_id)?;
            validate_text("activation request id", &command.activation_request_id)?;
            validate_text("entity type", &command.entity_type)?;
            validate_text("entity id", &command.entity_id)?;
            validate_text("canonical state", &command.canonical_initial_fields_json)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ReserveBootstrap)?;
            let operation_key = (
                command.tenant.clone(),
                command.caller_authority.clone(),
                command.idempotency_key.clone(),
            );
            if let Some(existing) = inner
                .schema_deployments
                .bootstraps
                .get(&operation_key)
                .cloned()
            {
                if existing.command.request_digest != command.request_digest {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                if existing.status == SchemaBootstrapStatus::Completed
                    && existing.creation_sequence.is_none()
                {
                    return Ok(ReserveSchemaBootstrapOutcome::Replayed(existing));
                }
                let target_key = (
                    command.tenant.clone(),
                    existing.pin.clone(),
                    command.entity_type.clone(),
                    command.entity_id.clone(),
                );
                if inner.schema_deployments.bootstrap_targets.get(&target_key)
                    != Some(&operation_key)
                {
                    return Err(SchemaDeploymentStoreError::BootstrapTargetConflict);
                }
                return Ok(ReserveSchemaBootstrapOutcome::Replayed(existing));
            }

            let mut pointers = inner.schema_deployments.active.values().filter(|pointer| {
                pointer.tenant == command.tenant
                    && pointer.accepted_request_id == command.activation_request_id
            });
            let pointer = pointers
                .next()
                .cloned()
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if pointers.next().is_some() {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "activation request identity is not unique".into(),
                ));
            }
            let deployment = inner
                .schema_deployments
                .deployments
                .get(&deployment_key(
                    &command.tenant,
                    &pointer.scope,
                    &pointer.bundle_digest,
                ))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if deployment.status != SchemaDeploymentStatus::Active
                || deployment.activation_pointer.as_ref() != Some(&pointer)
            {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            let pin = SchemaExecutionPin {
                scope: pointer.scope,
                bundle_digest: pointer.bundle_digest,
            };
            let target_key = (
                command.tenant.clone(),
                pin.clone(),
                command.entity_type.clone(),
                command.entity_id.clone(),
            );
            if inner
                .schema_deployments
                .bootstrap_targets
                .contains_key(&target_key)
            {
                return Err(SchemaDeploymentStoreError::BootstrapTargetConflict);
            }
            let operation = SchemaBootstrapOperation {
                command,
                pin,
                status: SchemaBootstrapStatus::Reserved,
                creation_sequence: None,
                action_failure: None,
                receipt: None,
                committed_sequence: 1,
            };
            inner
                .schema_deployments
                .bootstrap_targets
                .insert(target_key, operation_key.clone());
            inner
                .schema_deployments
                .bootstraps
                .insert(operation_key, operation.clone());
            Ok(ReserveSchemaBootstrapOutcome::Reserved(operation))
        }

        async fn record_schema_bootstrap_action_failure(
            &self,
            command: RecordSchemaBootstrapActionFailure,
        ) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
            validate_schema_bootstrap_failure(&command.failure)
                .map_err(SchemaDeploymentStoreError::InvalidInput)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(
                &mut inner,
                SimSchemaFaultPoint::RecordBootstrapActionFailure,
            )?;
            let operation = inner
                .schema_deployments
                .bootstraps
                .get_mut(&(
                    command.tenant,
                    command.caller_authority,
                    command.idempotency_key,
                ))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if operation.action_failure.as_ref() == Some(&command.failure) {
                return Ok(operation.clone());
            }
            if operation.status == SchemaBootstrapStatus::Completed {
                return Ok(operation.clone());
            }
            if operation.status != SchemaBootstrapStatus::Created
                || operation.committed_sequence != command.expected_sequence
            {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            operation.action_failure = Some(command.failure);
            operation.committed_sequence =
                checked_next(operation.committed_sequence, "bootstrap operation sequence")?;
            Ok(operation.clone())
        }

        async fn get_schema_bootstrap(
            &self,
            tenant: &str,
            caller_authority: &str,
            idempotency_key: &str,
        ) -> Result<Option<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .bootstraps
                .get(&(
                    tenant.to_string(),
                    caller_authority.to_string(),
                    idempotency_key.to_string(),
                ))
                .cloned())
        }

        async fn list_incomplete_schema_bootstraps(
            &self,
            limit: usize,
        ) -> Result<Vec<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
            if limit == 0 || limit > 1_024 {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "bootstrap recovery page budget must be between 1 and 1024".into(),
                ));
            }
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .bootstraps
                .values()
                .filter(|operation| operation.status != SchemaBootstrapStatus::Completed)
                .take(limit)
                .cloned()
                .collect())
        }

        async fn record_schema_bootstrap_created(
            &self,
            command: RecordSchemaBootstrapCreated,
        ) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::RecordBootstrapCreated)?;
            let key = (
                command.tenant,
                command.caller_authority,
                command.idempotency_key,
            );
            let operation = inner
                .schema_deployments
                .bootstraps
                .get_mut(&key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if operation.status == SchemaBootstrapStatus::Created
                && operation.creation_sequence == Some(command.creation_sequence)
            {
                return Ok(operation.clone());
            }
            if operation.status == SchemaBootstrapStatus::Completed
                && operation.creation_sequence == Some(command.creation_sequence)
            {
                return Ok(operation.clone());
            }
            if operation.status != SchemaBootstrapStatus::Reserved {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if operation.committed_sequence != command.expected_sequence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            operation.status = SchemaBootstrapStatus::Created;
            operation.creation_sequence = Some(command.creation_sequence);
            operation.committed_sequence =
                checked_next(operation.committed_sequence, "bootstrap operation sequence")?;
            Ok(operation.clone())
        }

        async fn complete_schema_bootstrap(
            &self,
            command: CompleteSchemaBootstrap,
        ) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
            validate_schema_bootstrap_receipt(&command.receipt)
                .map_err(SchemaDeploymentStoreError::InvalidInput)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::CompleteBootstrap)?;
            let key = (
                command.tenant,
                command.caller_authority,
                command.idempotency_key,
            );
            let operation = inner
                .schema_deployments
                .bootstraps
                .get_mut(&key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if operation.status == SchemaBootstrapStatus::Completed {
                return Ok(operation.clone());
            }
            if operation.committed_sequence != command.expected_sequence
                || command.receipt.pin != operation.pin
                || command.receipt.entity_type != operation.command.entity_type
                || command.receipt.entity_id != operation.command.entity_id
                || command.receipt.creation_sequence != operation.creation_sequence
                || operation.action_failure.is_some()
                    && command.receipt.failure != operation.action_failure
            {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let release_target = command.receipt.creation_sequence.is_none();
            operation.status = SchemaBootstrapStatus::Completed;
            operation.receipt = Some(command.receipt);
            operation.committed_sequence =
                checked_next(operation.committed_sequence, "bootstrap operation sequence")?;
            let completed = operation.clone();
            if release_target {
                let target_key = (
                    completed.command.tenant.clone(),
                    completed.pin.clone(),
                    completed.command.entity_type.clone(),
                    completed.command.entity_id.clone(),
                );
                inner
                    .schema_deployments
                    .bootstrap_targets
                    .remove(&target_key);
            }
            Ok(completed)
        }
    };
}
