use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(super) async fn run_migration_batch(
        &self,
        mut job: SchemaMigrationJob,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let tenant = job.command.tenant.clone();
        let scope = job.command.scope.clone();
        let tenant_id = TenantId::new(&tenant);
        if job.status == SchemaMigrationStatus::Completed {
            return Ok(migration_receipt(&job));
        }
        if job.status == SchemaMigrationStatus::Submitted
            || (job.status == SchemaMigrationStatus::Migrating
                && job
                    .lease_expires_at
                    .is_some_and(|deadline| simulated_millis().is_ok_and(|now| now >= deadline)))
        {
            let claim_from = migration_status_name(job.status);
            let now = simulated_millis()?;
            let lease = now.checked_add(120_000).ok_or_else(|| {
                ServiceError::new("migration_failed", "migration lease exhausted", true)
            })?;
            job = self
                .store()?
                .claim_schema_migration(&tenant, &job.command.job_id, now, lease)
                .await
                .map_err(ServiceError::from_store)?;
            emit_schema_lifecycle(
                &tenant,
                "SchemaMigration",
                &job.command.job_id,
                "claim",
                claim_from,
                "migrating",
                &scope,
            );
        }
        if job.status == SchemaMigrationStatus::Migrating {
            let target = self
                .store()?
                .get_schema_deployment(&tenant, &scope, &job.command.target_bundle_digest)
                .await
                .map_err(ServiceError::from_store)?
                .ok_or_else(|| {
                    ServiceError::new("migration_rejected", "target bundle not found", false)
                })?;
            let source = self
                .store()?
                .get_schema_deployment(&tenant, &scope, &job.command.source_bundle_digest)
                .await
                .map_err(ServiceError::from_store)?
                .ok_or_else(|| {
                    ServiceError::new("migration_rejected", "source bundle not found", false)
                })?;
            self.stage_registry_bundle(&target)?;
            self.stage_registry_bundle(&source)?;
            let page_budget = migration_budget_usize(
                job.command.budgets.entities_per_batch.into(),
                "entities_per_batch",
            )?;
            let source_entity_types = source
                .bundle
                .canonical_ioa
                .keys()
                .map(|qualified| {
                    qualified
                        .rsplit('.')
                        .next()
                        .unwrap_or(qualified)
                        .to_string()
                })
                .collect::<Vec<_>>();
            let (journal, _) = self.state.event_journal().ok_or_else(|| {
                ServiceError::new(
                    "backend_unavailable",
                    "schema migration requires a durable event journal",
                    true,
                )
            })?;
            let pass_write_version = if job.scan_cursor.is_none() {
                journal
                    .scoped_bundle_write_version(
                        &tenant,
                        &job.command.scope,
                        &job.command.source_bundle_digest,
                    )
                    .await
                    .map_err(|error| {
                        ServiceError::new("migration_failed", error.to_string(), true)
                    })?
            } else {
                job.catch_up_sequence
            };
            let candidates = self
                .state
                .page_scoped_entity_ids(
                    &tenant_id,
                    &source_entity_types,
                    &SchemaExecutionPin {
                        scope: job.command.scope.clone(),
                        bundle_digest: job.command.source_bundle_digest.clone(),
                    },
                    job.scan_cursor
                        .as_ref()
                        .map(|cursor| (cursor.0.as_str(), cursor.1.as_str())),
                    page_budget.saturating_add(1),
                )
                .await
                .map_err(|error| ServiceError::new("migration_failed", error, true))?;
            let scan_complete = candidates.len() <= page_budget;
            let rows_to_transform = candidates.into_iter().take(page_budget).collect::<Vec<_>>();
            let page_cursor = rows_to_transform
                .last()
                .map(|(entity_type, entity_id)| (entity_type.clone(), entity_id.clone()))
                .or_else(|| job.scan_cursor.clone());
            let shadow_budget =
                migration_budget_usize(job.command.budgets.total_entities, "total_entities")?
                    .saturating_add(1);
            let existing_shadow = self
                .store()?
                .page_schema_migration_shadow(&tenant, &job.command.job_id, None, shadow_budget)
                .await
                .map_err(ServiceError::from_store)?
                .into_iter()
                .map(|row| ((row.entity_type.clone(), row.entity_id.clone()), row))
                .collect::<BTreeMap<_, _>>();
            let engine_hash = job
                .command
                .module_digest
                .strip_prefix("sha256:")
                .ok_or_else(|| {
                    ServiceError::new("migration_rejected", "invalid module digest", false)
                })?;
            self.state
                .ensure_wasm_module_cached(&tenant_id, &job.command.module_name, engine_hash)
                .await
                .map_err(|error| ServiceError::new("migration_failed", error, true))?;
            let limits = migration_limits_from_job(&job)?;
            let mut shadow_rows = Vec::new();
            for (item_index, (entity_type, entity_id)) in rows_to_transform.into_iter().enumerate()
            {
                let source_state = self
                    .state
                    .get_scoped_entity_state(
                        &tenant_id,
                        &entity_type,
                        &entity_id,
                        SchemaExecutionPin {
                            scope: scope.clone(),
                            bundle_digest: job.command.source_bundle_digest.clone(),
                        },
                    )
                    .await
                    .map_err(|error| ServiceError::new("migration_failed", error, true))?;
                if existing_shadow
                    .get(&(entity_type.clone(), entity_id.clone()))
                    .is_some_and(|row| row.source_sequence == source_state.state.sequence_nr)
                {
                    continue;
                }
                let mut migratable_fields = source_state.state.fields.clone();
                if let Some(fields) = migratable_fields.as_object_mut() {
                    fields.remove(crate::entity_actor::SCHEMA_PIN_FIELD);
                    collapse_runtime_alias(fields, "Id", "id")?;
                    collapse_runtime_alias(fields, "Status", "status")?;
                }
                let canonical_state_json = canonical_json_object(&migratable_fields)?;
                let input = SchemaMigrationInputV1 {
                    abi_version: 1,
                    source_bundle_digest: job.command.source_bundle_digest.clone(),
                    target_bundle_digest: job.command.target_bundle_digest.clone(),
                    entity_type: entity_type.clone(),
                    entity_id: entity_id.clone(),
                    source_sequence: source_state.state.sequence_nr,
                    canonical_state_json: canonical_state_json.clone(),
                    logical_context: SchemaMigrationLogicalContextV1 {
                        batch_id: format!(
                            "{}:{}",
                            job.command.job_id,
                            job.consumed_batches.saturating_add(1)
                        ),
                        item_index: u32::try_from(item_index).map_err(|_| {
                            ServiceError::new(
                                "migration_budget_exhausted",
                                "migration batch index exhausted",
                                false,
                            )
                        })?,
                    },
                };
                let output = self
                    .state
                    .wasm_engine
                    .invoke_pure_migration(engine_hash, &input, limits)
                    .map_err(ServiceError::from_migration)?;
                let output_digest = digest_json(&output)?;
                let target_state_json = match output {
                    SchemaMigrationOutputV1::Unchanged => canonical_state_json,
                    SchemaMigrationOutputV1::Replace {
                        canonical_state_json,
                    } => canonical_state_json,
                    SchemaMigrationOutputV1::Reject { code, message } => {
                        return Err(ServiceError::new(
                            "migration_rejected",
                            format!("migration rejected entity with {code}: {message}"),
                            false,
                        ));
                    }
                };
                let target_fields: serde_json::Value = serde_json::from_str(&target_state_json)
                    .map_err(|error| {
                        ServiceError::new("migration_rejected", error.to_string(), false)
                    })?;
                let target_state_json = canonical_json_object(&target_fields)?;
                let target_pin = SchemaExecutionPin {
                    scope: scope.clone(),
                    bundle_digest: job.command.target_bundle_digest.clone(),
                };
                let target_table = self
                    .state
                    .registry
                    .read()
                    .map_err(|_| {
                        ServiceError::new("migration_failed", "registry lock poisoned", true)
                    })?
                    .get_scoped_table_at_digest(
                        &tenant_id,
                        &target_pin.scope,
                        &target_pin.bundle_digest,
                        &entity_type,
                    )
                    .ok_or_else(|| {
                        ServiceError::new(
                            "migration_rejected",
                            format!("target bundle has no entity type '{entity_type}'"),
                            false,
                        )
                    })?;
                let target_initial_state = target_table.initial_state.clone();
                let target_status =
                    super::validation::unambiguous_alias_value(&target_fields, "Status")?
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(&target_initial_state)
                        .to_string();
                if !target_table.states.contains(&target_status) {
                    return Err(ServiceError::new(
                        "migration_rejected",
                        format!(
                            "target Status '{target_status}' is not declared by the target IOA"
                        ),
                        false,
                    ));
                }
                let target_persistence_id = format!(
                    "{tenant}:{entity_type}:{}",
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        &entity_id,
                        &target_pin,
                    )
                );
                let target_sequence = journal
                    .read_latest_events(&target_persistence_id, 1)
                    .await
                    .map_err(|error| {
                        ServiceError::new("migration_failed", error.to_string(), true)
                    })?
                    .last()
                    .map_or(0, |event| event.sequence_nr);
                let action = crate::entity_actor::types::FIELD_UPDATE_EVENT_TYPE;
                let event = EntityEvent {
                    action: action.into(),
                    from_status: target_initial_state.clone(),
                    to_status: target_status,
                    timestamp: sim_now(),
                    params: serde_json::json!({
                        "replace": true,
                        "migration": true,
                        "fields": target_fields,
                    }),
                    idempotency_key: Some(format!(
                        "schema-migration:{}:{}",
                        job.command.job_id, input.source_sequence
                    )),
                };
                let mut payload = serde_json::to_value(&event).map_err(|error| {
                    ServiceError::new("migration_rejected", error.to_string(), false)
                })?;
                payload
                    .as_object_mut()
                    .ok_or_else(|| {
                        ServiceError::new(
                            "migration_rejected",
                            "serialized migration event was not an object",
                            false,
                        )
                    })?
                    .insert(
                        crate::entity_actor::SCHEMA_PIN_FIELD.into(),
                        serde_json::to_value(crate::entity_actor::schema_event_pin(
                            &target_pin,
                            &entity_type,
                            action,
                        ))
                        .map_err(|error| {
                            ServiceError::new("migration_rejected", error.to_string(), false)
                        })?,
                    );
                let event_id = temper_runtime::scheduler::sim_uuid();
                shadow_rows.push(SchemaMigrationShadowRow {
                    entity_type,
                    entity_id,
                    source_sequence: input.source_sequence,
                    canonical_state_json: target_state_json,
                    input_digest: digest_json(&input)?,
                    output_digest,
                    target_event: PersistenceEnvelope {
                        sequence_nr: target_sequence.saturating_add(1),
                        event_type: action.into(),
                        payload,
                        metadata: EventMetadata {
                            event_id,
                            causation_id: event_id,
                            correlation_id: event_id,
                            timestamp: sim_now(),
                            actor_id: format!("schema-migration:{}", job.command.job_id),
                        },
                    },
                });
            }
            let end_write_version = if scan_complete {
                journal
                    .scoped_bundle_write_version(
                        &tenant,
                        &job.command.scope,
                        &job.command.source_bundle_digest,
                    )
                    .await
                    .map_err(|error| {
                        ServiceError::new("migration_failed", error.to_string(), true)
                    })?
            } else {
                pass_write_version
            };
            let stable_complete = scan_complete && end_write_version == pass_write_version;
            let next_cursor = if scan_complete && !stable_complete {
                None
            } else {
                page_cursor
            };
            let input_digest = digest_json(
                &shadow_rows
                    .iter()
                    .map(|row| row.input_digest.as_str())
                    .collect::<Vec<_>>(),
            )?;
            let output_digest = digest_json(
                &shadow_rows
                    .iter()
                    .map(|row| row.output_digest.as_str())
                    .collect::<Vec<_>>(),
            )?;
            let receipt_id = format!(
                "batch:{}:{}",
                job.command.job_id,
                job.consumed_batches.saturating_add(1)
            );
            let committed_targets = shadow_rows
                .iter()
                .map(|row| (row.entity_type.clone(), row.entity_id.clone()))
                .collect::<Vec<_>>();
            job = self
                .store()?
                .commit_schema_migration_batch(
                    &tenant,
                    CommitSchemaMigrationBatch {
                        job_id: job.command.job_id.clone(),
                        expected_fence: job.fence,
                        expected_cursor: job.scan_cursor.clone(),
                        next_cursor: next_cursor.clone(),
                        scan_complete: stable_complete,
                        restart_scan: scan_complete && !stable_complete,
                        observed_source_write_version: end_write_version,
                        receipt: SchemaMigrationBatchReceipt {
                            id: receipt_id,
                            source_cursor: job.scan_cursor.clone(),
                            next_cursor,
                            input_digest,
                            output_digest,
                            row_count: u32::try_from(shadow_rows.len()).map_err(|_| {
                                ServiceError::new(
                                    "migration_budget_exhausted",
                                    "migration batch size exhausted",
                                    false,
                                )
                            })?,
                        },
                        rows: shadow_rows,
                    },
                )
                .await
                .map_err(ServiceError::from_store)?;
            emit_schema_lifecycle(
                &tenant,
                "SchemaMigration",
                &job.command.job_id,
                "commit_batch",
                "migrating",
                migration_status_name(job.status),
                &scope,
            );
            for (entity_type, entity_id) in committed_targets {
                self.state.stop_and_remove_scoped_entity(
                    &tenant_id,
                    &entity_type,
                    &entity_id,
                    &SchemaExecutionPin {
                        scope: job.command.scope.clone(),
                        bundle_digest: job.command.target_bundle_digest.clone(),
                    },
                );
            }
            if !job.scan_complete {
                return Ok(migration_receipt(&job));
            }
        }
        self.finish_migration(job, &tenant, &scope, &tenant_id)
            .await
    }
}

pub(super) fn collapse_runtime_alias(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    canonical: &str,
    generated: &str,
) -> Result<(), ServiceError> {
    match (fields.get(canonical), fields.get(generated)) {
        (Some(canonical_value), Some(generated_value)) if canonical_value != generated_value => {
            Err(ServiceError::new(
                "migration_rejected",
                format!("runtime-owned aliases '{canonical}' and '{generated}' disagree"),
                false,
            ))
        }
        (Some(_), Some(_)) => {
            fields.remove(generated);
            Ok(())
        }
        _ => Ok(()),
    }
}
