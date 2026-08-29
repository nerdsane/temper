macro_rules! impl_schema_retire_method {
    () => {
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
