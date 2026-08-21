use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(super) async fn finish_migration(
        &self,
        mut job: SchemaMigrationJob,
        tenant: &str,
        scope: &SchemaScope,
        tenant_id: &TenantId,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        if job.status == SchemaMigrationStatus::Validating {
            let total_entity_budget =
                migration_budget_usize(job.command.budgets.total_entities, "total_entities")?;
            let rows = self
                .store()?
                .page_schema_migration_shadow(
                    tenant,
                    &job.command.job_id,
                    None,
                    total_entity_budget.saturating_add(1),
                )
                .await
                .map_err(ServiceError::from_store)?;
            if rows.len() > total_entity_budget {
                return Err(ServiceError::new(
                    "migration_budget_exhausted",
                    "migration validation page exceeded total entity budget",
                    false,
                ));
            }
            for row in &rows {
                let target = self
                    .state
                    .get_scoped_migration_shadow_state(
                        tenant_id,
                        &row.entity_type,
                        &row.entity_id,
                        SchemaExecutionPin {
                            scope: scope.clone(),
                            bundle_digest: job.command.target_bundle_digest.clone(),
                        },
                    )
                    .await
                    .map_err(|error| ServiceError::new("migration_failed", error, true))?;
                let mut durable_fields = target.state.fields;
                if let Some(fields) = durable_fields.as_object_mut() {
                    fields.remove(crate::entity_actor::SCHEMA_PIN_FIELD);
                    super::runner::collapse_runtime_alias(fields, "Id", "id")?;
                    super::runner::collapse_runtime_alias(fields, "Status", "status")?;
                }
                if target.state.sequence_nr != row.target_event.sequence_nr
                    || canonical_json_object(&durable_fields)? != row.canonical_state_json
                {
                    return Err(ServiceError::new(
                        "migration_rejected",
                        "durable target journal differs from committed shadow output",
                        false,
                    ));
                }
                self.validate_migrated_target_state(
                    tenant_id,
                    scope,
                    &job.command.target_bundle_digest,
                    &row.entity_type,
                    &durable_fields,
                )
                .await?;
            }
            let shadow_digest = digest_json(&rows)?;
            let validation_id = format!("validation:{}", &shadow_digest[7..23]);
            job = self
                .store()?
                .validate_schema_migration(
                    tenant,
                    &job.command.job_id,
                    job.fence,
                    SchemaMigrationValidationReceipt {
                        id: validation_id,
                        shadow_digest,
                        caught_up_sequence: job.catch_up_sequence,
                        passed: true,
                    },
                )
                .await
                .map_err(ServiceError::from_store)?;
            emit_schema_lifecycle(
                tenant,
                "SchemaMigration",
                &job.command.job_id,
                "validate",
                "validating",
                "ready",
                scope,
            );
        }
        if job.status == SchemaMigrationStatus::Ready {
            let validation_receipt_id = job.validation_receipt_id.clone().ok_or_else(|| {
                ServiceError::new(
                    "migration_rejected",
                    "ready migration has no validation receipt",
                    false,
                )
            })?;
            self.store()?
                .cut_over_schema_migration(
                    tenant,
                    &job.command.job_id,
                    job.fence,
                    &validation_receipt_id,
                )
                .await
                .map_err(ServiceError::from_store)?;
            job = self
                .store()?
                .get_schema_migration(tenant, &job.command.job_id)
                .await
                .map_err(ServiceError::from_store)?
                .ok_or_else(|| {
                    ServiceError::new("migration_failed", "migration disappeared", true)
                })?;
            emit_schema_lifecycle(
                tenant,
                "SchemaMigration",
                &job.command.job_id,
                "cut_over",
                "ready",
                "cut_over",
                scope,
            );
        }
        if job.status == SchemaMigrationStatus::CutOver {
            self.recover_registry_pointer(tenant, scope).await?;
            let rows = self
                .store()?
                .page_schema_migration_shadow(
                    tenant,
                    &job.command.job_id,
                    None,
                    migration_budget_usize(job.command.budgets.total_entities, "total_entities")?,
                )
                .await
                .map_err(ServiceError::from_store)?;
            for row in rows {
                self.state.stop_and_remove_scoped_entity(
                    tenant_id,
                    &row.entity_type,
                    &row.entity_id,
                    &SchemaExecutionPin {
                        scope: job.command.scope.clone(),
                        bundle_digest: job.command.source_bundle_digest.clone(),
                    },
                );
            }
            job = self
                .store()?
                .complete_schema_migration(tenant, &job.command.job_id, job.fence)
                .await
                .map_err(ServiceError::from_store)?;
            emit_schema_lifecycle(
                tenant,
                "SchemaMigration",
                &job.command.job_id,
                "complete",
                "cut_over",
                "completed",
                scope,
            );
        }
        Ok(migration_receipt(&job))
    }
}
