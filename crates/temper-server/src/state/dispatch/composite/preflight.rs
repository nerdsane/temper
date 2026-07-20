//! Composite sub-write preparation and authorization.

use super::*;

impl crate::state::ServerState {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare_composite_sub_writes(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        sub_writes: &[CompositeSubWrite],
        metadata: &CompositeActionMetadata,
        composite_agent_ctx: &AgentContext,
        parent_idempotency: &str,
    ) -> Result<Vec<PreparedCompositeSubWrite>, DispatchError> {
        let sub_security_ctx = composite_agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            DispatchError::Internal(
                "composite sub-write authorization requires a security context".to_string(),
            )
        })?;
        let mut prepared = Vec::with_capacity(sub_writes.len());
        let mut governed_cache = BTreeMap::new();
        let mut create_auth_defaults_cache = BTreeMap::new();

        for (idx, sub_write) in sub_writes.iter().cloned().enumerate() {
            let sub_entity_type = sub_write.entity_type.clone();
            let sub_entity_id = sub_write.entity_id.clone();
            let sub_action = sub_write.action.clone();
            let sub_params = normalize_sub_write_params(sub_write);

            let governed = match governed_cache.get(&sub_entity_type) {
                Some(governed) => *governed,
                None => {
                    let governed = self
                        .is_entity_type_governed(tenant, &sub_entity_type)
                        .map_err(DispatchError::Internal)?;
                    governed_cache.insert(sub_entity_type.clone(), governed);
                    governed
                }
            };
            if !governed {
                return Err(DispatchError::Ungoverned(sub_entity_type));
            }

            let use_parent_gate =
                composite_sub_write_uses_parent_gate(metadata, &sub_entity_type, &sub_action);
            let resource_attrs = if use_parent_gate {
                None
            } else if sub_action == "Create" {
                if !create_auth_defaults_cache.contains_key(&sub_entity_type) {
                    create_auth_defaults_cache.insert(
                        sub_entity_type.clone(),
                        self.composite_create_auth_defaults(tenant, &sub_entity_type)?,
                    );
                }
                let defaults = create_auth_defaults_cache
                    .get(&sub_entity_type)
                    .expect("create auth defaults inserted before use");
                Some(composite_create_resource_attrs_from_defaults(
                    &sub_entity_id,
                    &sub_params,
                    defaults,
                ))
            } else {
                Some(
                    self.composite_sub_write_auth_resource_attrs(
                        tenant,
                        &sub_entity_type,
                        &sub_entity_id,
                        &sub_action,
                        &sub_params,
                    )
                    .await?,
                )
            };

            if let Some(resource_attrs) = resource_attrs {
                self.authorize_with_context(
                    sub_security_ctx,
                    &sub_action,
                    &sub_entity_type,
                    &resource_attrs,
                    tenant.as_str(),
                )
                .map_err(|denial| {
                    DispatchError::AuthzDenied(format!(
                        "composite {entity_type}.{action} sub-write {idx} denied for {sub_entity_type}.{sub_action}: {denial}"
                    ))
                })?;
            }

            prepared.push(PreparedCompositeSubWrite {
                idx,
                entity_type: sub_entity_type,
                entity_id: sub_entity_id,
                action: sub_action,
                params: sub_params,
                idempotency_key: format!(
                    "composite:{tenant}:{entity_type}:{entity_id}:{action}:{parent_idempotency}:subwrite:{idx}"
                ),
                preflight_target: None,
                uses_parent_gate: use_parent_gate,
            });
        }

        let known_absent_create_targets = self
            .composite_known_absent_create_targets(tenant, &prepared)
            .await?;

        for write in &mut prepared {
            let known_absent_create = known_absent_create_targets
                .contains(&(write.entity_type.clone(), write.entity_id.clone()));
            write.preflight_target = Some(
                self.preflight_composite_sub_write_transition(
                    tenant,
                    entity_type,
                    action,
                    write,
                    known_absent_create,
                )
                .await?,
            );
        }

        let storage_writes = prepared
            .iter()
            .map(|write| CommonsStorageWrite {
                entity_type: write.entity_type.clone(),
                entity_id: write.entity_id.clone(),
                action: write.action.clone(),
                fields: write.params.clone(),
            })
            .collect::<Vec<_>>();
        for write in &storage_writes {
            self.enforce_commons_verified_owner_for_write(
                tenant,
                &write.entity_type,
                &write.fields,
            )
            .await
            .map_err(composite_account_verification_error)?;
            self.enforce_commons_app_name_unique_for_write(
                tenant,
                &write.entity_type,
                &write.entity_id,
                &write.fields,
            )
            .await
            .map_err(composite_app_uniqueness_error)?;
        }
        self.enforce_commons_storage_caps_for_writes(tenant, &storage_writes)
            .await
            .map_err(composite_storage_cap_error)?;

        Ok(prepared)
    }

    pub(super) async fn composite_known_absent_create_targets(
        &self,
        tenant: &TenantId,
        prepared: &[PreparedCompositeSubWrite],
    ) -> Result<BTreeSet<(String, String)>, DispatchError> {
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(BTreeSet::new());
        };

        let mut by_type: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for write in prepared {
            if write.uses_parent_gate
                || write.action != "Create"
                || self.entity_exists(tenant, &write.entity_type, &write.entity_id)
            {
                continue;
            }
            by_type
                .entry(write.entity_type.clone())
                .or_default()
                .insert(write.entity_id.clone());
        }

        let mut absent = BTreeSet::new();
        for (entity_type, ids) in by_type {
            let entity_ids = ids.into_iter().collect::<Vec<_>>();
            let Some(rows) = query_plane
                .load_entity_catalog_rows(tenant.as_str(), &entity_type, &entity_ids)
                .await
                .map_err(|e| {
                    DispatchError::Internal(format!(
                        "query projection preflight failed for composite {entity_type} creates: {e}"
                    ))
                })?
            else {
                continue;
            };

            let present = rows
                .into_iter()
                .map(|row| row.entity_id)
                .collect::<BTreeSet<_>>();
            for entity_id in entity_ids {
                if !present.contains(&entity_id) {
                    absent.insert((entity_type.clone(), entity_id));
                }
            }
        }

        Ok(absent)
    }

    pub(super) async fn preflight_composite_sub_write_transition(
        &self,
        tenant: &TenantId,
        parent_entity_type: &str,
        parent_action: &str,
        write: &PreparedCompositeSubWrite,
        known_absent_create: bool,
    ) -> Result<PreflightCompositeTarget, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
        let known_absent_create = known_absent_create
            && write.action == "Create"
            && !self.entity_exists(tenant, &write.entity_type, &write.entity_id);
        let target_exists = if known_absent_create {
            false
        } else {
            self.ensure_entity_loaded(tenant, &write.entity_type, &write.entity_id)
                .await
        };
        let target_state = if known_absent_create {
            synthetic_initial_state(&write.entity_type, &write.entity_id, &table)
        } else if target_exists {
            self.get_tenant_entity_state(tenant, &write.entity_type, &write.entity_id)
                .await
                .map_err(DispatchError::Internal)?
                .state
        } else {
            synthetic_initial_state(&write.entity_type, &write.entity_id, &table)
        };

        let preflight_target = PreflightCompositeTarget {
            target_existed: target_exists,
            state: target_state.clone(),
        };

        if target_state.has_processed_idempotency_key(&write.idempotency_key) {
            return Ok(preflight_target);
        }

        validate_composite_ref_preflight_compare_and_set(
            parent_entity_type,
            parent_action,
            write,
            &preflight_target,
        )?;

        if !target_state.can_accept_event() {
            return Err(DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would exceed the event budget for {}:{}",
                write.idx, write.entity_type, write.entity_id
            )));
        }

        let cross_entity_booleans =
            if table_has_cross_entity_guards_for_action(&table, &write.action) {
                self.resolve_cross_entity_guards(
                    tenant,
                    &write.entity_type,
                    &write.entity_id,
                    &write.action,
                )
                .await
            } else {
                BTreeMap::new()
            };
        let eval_ctx = build_eval_context_with_xref(&target_state, &cross_entity_booleans);
        match table.evaluate_ctx(&target_state.status, &eval_ctx, &write.action) {
            Some(result) if result.success => Ok(preflight_target),
            Some(_) => Err(DispatchError::Conflict(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would fail: action '{}' is not valid from state '{}'",
                write.idx, write.action, target_state.status
            ))),
            None => Err(DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would fail: unknown action '{}'",
                write.idx, write.action
            ))),
        }
    }

    pub(super) async fn composite_sub_write_auth_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: &Value,
    ) -> Result<BTreeMap<String, Value>, DispatchError> {
        if action == "Create" {
            return self.composite_create_resource_attrs(tenant, entity_type, entity_id, params);
        }

        if !self
            .ensure_entity_loaded(tenant, entity_type, entity_id)
            .await
        {
            return Err(DispatchError::Internal(format!(
                "composite sub-write target {entity_type}:{entity_id} does not exist"
            )));
        }

        self.load_authz_resource_snapshot(tenant, entity_type, entity_id)
            .await
            .map(|snapshot| snapshot.resource_attrs)
            .map_err(DispatchError::Internal)
    }

    pub(super) fn composite_create_auth_defaults(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<CompositeCreateAuthDefaults, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let has_spec = self
            .has_registered_spec(tenant, entity_type)
            .map_err(DispatchError::Internal)?;
        Ok(CompositeCreateAuthDefaults {
            initial_state: table.initial_state.clone(),
            has_spec,
        })
    }

    pub(super) fn composite_create_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        params: &Value,
    ) -> Result<BTreeMap<String, Value>, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let mut resource_attrs = BTreeMap::new();
        resource_attrs.insert("id".to_string(), Value::String(entity_id.to_string()));
        resource_attrs.insert(
            "status".to_string(),
            Value::String(table.initial_state.clone()),
        );
        if let Value::Object(fields) = params {
            for (key, value) in fields {
                resource_attrs.insert(key.clone(), value.clone());
            }
        }
        let has_spec = self
            .has_registered_spec(tenant, entity_type)
            .map_err(DispatchError::Internal)?;
        resource_attrs.insert("has_spec".to_string(), Value::Bool(has_spec));
        Ok(resource_attrs)
    }

    pub(super) fn transition_table_for_dispatch(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<Arc<TransitionTable>, DispatchError> {
        if let Some(table) = self
            .registry
            .read()
            .map_err(|e| DispatchError::Internal(format!("registry lock poisoned: {e}")))?
            .get_table(tenant, entity_type)
        {
            return Ok(table);
        }

        self.transition_tables
            .get(entity_type)
            .cloned()
            .ok_or_else(|| DispatchError::Ungoverned(entity_type.to_string()))
    }

    #[allow(dead_code)]
    pub(super) async fn ensure_composite_entry_transition_allowed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> Result<(), DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let current = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .map_err(DispatchError::Internal)?;
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;
        let eval_ctx = build_eval_context_with_xref(&current.state, &cross_entity_booleans);

        match table.evaluate_ctx(&current.state.status, &eval_ctx, action) {
            Some(result) if result.success => Ok(()),
            Some(_) => Err(DispatchError::Internal(format!(
                "Composite action '{action}' not valid from state '{}'",
                current.state.status
            ))),
            None => Err(DispatchError::Internal(format!(
                "Unknown composite action: {action}"
            ))),
        }
    }
}
