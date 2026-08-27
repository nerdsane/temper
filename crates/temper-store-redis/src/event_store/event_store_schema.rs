macro_rules! redis_event_store_schema_methods {
    () => {
    async fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = Self::tenant_journals_key(tenant);
        let (min, max) = match entity_type {
            Some(wanted) => {
                if after.is_some_and(|(after_type, _)| after_type > wanted) {
                    return Ok(Vec::new());
                }
                let prefix = format!("{}!", encode_lex_component(wanted));
                let min = match after {
                    Some((after_type, after_id)) if after_type == wanted => {
                        format!("({}", Self::journal_member(after_type, after_id))
                    }
                    _ => format!("[{prefix}"),
                };
                (min, format!("[{prefix}~"))
            }
            None => (
                after.map_or_else(
                    || "-".to_string(),
                    |(after_type, after_id)| {
                        format!("({}", Self::journal_member(after_type, after_id))
                    },
                ),
                "+".to_string(),
            ),
        };
        let count = limit.min(i64::MAX as usize) as i64;
        let members: Vec<String> = self
            .client
            .zrangebylex(&key, min, max, Some((0, count)))
            .await
            .map_err(storage_error)?;
        members
            .into_iter()
            .map(|member| Self::parse_journal_member(&member))
            .collect()
    }

    async fn list_unscoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        const RAW_PAGE_BUDGET: usize = 256;
        let complete_key = Self::unscoped_index_complete_key(tenant, entity_type);
        let complete: Option<String> = self.client.get(&complete_key).await.map_err(storage_error)?;
        if complete.as_deref() != Some("1") {
            let cursor_key = Self::unscoped_index_cursor_key(tenant, entity_type);
            let index_key = Self::unscoped_journals_key(tenant, entity_type);
            let cursor: Option<String> = self.client.get(&cursor_key).await.map_err(storage_error)?;
            let prefix = format!("{}!", encode_lex_component(entity_type));
            let min = cursor
                .as_ref()
                .map_or_else(|| format!("[{prefix}"), |value| format!("({value}"));
            let raw: Vec<String> = self
                .client
                .zrangebylex(
                    Self::tenant_journals_key(tenant),
                    min,
                    format!("[{prefix}~"),
                    Some((0, RAW_PAGE_BUDGET as i64)),
                )
                .await
                .map_err(storage_error)?;
            let done = raw.len() < RAW_PAGE_BUDGET;
            let next_cursor = raw.last().cloned().unwrap_or_else(|| cursor.clone().unwrap_or_default());
            let mut args = vec![
                cursor.unwrap_or_default(),
                next_cursor,
                if done { "1".into() } else { "0".into() },
            ];
            for member in raw {
                let (_, entity_id) = Self::parse_journal_member(&member)?;
                if split_scoped_journal_entity_id(&entity_id).is_none() {
                    args.push(encode_lex_component(&entity_id));
                }
            }
            let result: Vec<i64> = self
                .backfill_unscoped_index_script
                .evalsha_with_reload(
                    &self.client,
                    vec![cursor_key, index_key, complete_key],
                    args,
                )
                .await
                .map_err(storage_error)?;
            if result.as_slice() != [1] || !done {
                return Err(PersistenceError::Storage(
                    "Redis unscoped journal index backfill is pending".into(),
                ));
            }
        }
        let min = after_entity_id.map_or_else(
            || "-".to_string(),
            |entity_id| format!("({}", encode_lex_component(entity_id)),
        );
        let members: Vec<String> = self
            .client
            .zrangebylex(
                Self::unscoped_journals_key(tenant, entity_type),
                min,
                "+",
                Some((0, limit.min(i64::MAX as usize) as i64)),
            )
            .await
            .map_err(storage_error)?;
        members
            .into_iter()
            .map(|member| decode_lex_component(&member))
            .collect()
    }

    async fn unscoped_entity_type_write_version(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        let raw: Option<String> = self
            .client
            .get(Self::unscoped_generation_key(tenant, entity_type))
            .await
            .map_err(storage_error)?;
        raw.map_or(Ok(0), |value| {
            value.parse().map_err(|_| {
                PersistenceError::Serialization(
                    "invalid Redis unscoped publication generation".into(),
                )
            })
        })
    }

    async fn activate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        fence: &StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let StreamPublicationFence::InstalledApplication {
            application_id,
            semantic_digest,
            bindings,
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot activate global stream publications".into(),
            ));
        };
        let app_key = Self::unscoped_application_fence_key(tenant, application_id);
        let previous: Option<String> = self.client.get(&app_key).await.map_err(storage_error)?;
        let previous_fence = previous
            .as_deref()
            .map(serde_json::from_str::<StreamPublicationFence>)
            .transpose()
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let mut entity_types = std::collections::BTreeSet::new();
        entity_types.extend(bindings.keys().cloned());
        if let Some(StreamPublicationFence::InstalledApplication {
            bindings: previous_bindings,
            ..
        }) = previous_fence
        {
            entity_types.extend(previous_bindings.keys().cloned());
        }
        let mut keys = vec![app_key];
        let mut args = vec![previous.unwrap_or_default(), entity_types.len().to_string()];
        for entity_type in &entity_types {
            keys.push(Self::unscoped_generation_key(tenant, entity_type));
            keys.push(Self::unscoped_fence_key(tenant, entity_type));
            if let Some(binding) = bindings.get(entity_type) {
                args.push(binding.expected_write_version.to_string());
                args.push(
                    serde_json::to_string(&serde_json::json!({
                        "application_id": application_id,
                        "semantic_digest": semantic_digest,
                        "publication_action": binding.publication_action,
                        "capability_digest": binding.capability_digest,
                    }))
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                );
            } else {
                args.push("-1".into());
                args.push(String::new());
            }
        }
        args.push(
            serde_json::to_string(fence)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
        );
        let result: Vec<i64> = self
            .activate_unscoped_fence_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            [1] => Ok(()),
            [-2] => Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            )),
            [0, index, actual] => {
                let position = usize::try_from(*index)
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .ok_or_else(|| {
                        PersistenceError::Storage("invalid Redis fence conflict index".into())
                    })?;
                let entity_type = entity_types.iter().nth(position).ok_or_else(|| {
                    PersistenceError::Storage("invalid Redis fence conflict index".into())
                })?;
                Err(PersistenceError::ConcurrencyViolation {
                    expected: bindings
                        .get(entity_type)
                        .map_or(0, |binding| binding.expected_write_version),
                    actual: u64::try_from(*actual).map_err(|_| {
                        PersistenceError::Storage("invalid Redis fence generation".into())
                    })?,
                })
            }
            other => Err(PersistenceError::Storage(format!(
                "unexpected Redis fence activation result: {other:?}"
            ))),
        }
    }

    async fn deactivate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
        semantic_digest: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let app_key = Self::unscoped_application_fence_key(tenant, application_id);
        let previous: Option<String> = self.client.get(&app_key).await.map_err(storage_error)?;
        let Some(previous_raw) = previous else {
            return Ok(());
        };
        let previous_fence: StreamPublicationFence = serde_json::from_str(&previous_raw)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let StreamPublicationFence::InstalledApplication {
            semantic_digest: found_digest,
            bindings,
            ..
        } = previous_fence
        else {
            return Err(PersistenceError::Storage(
                "installed application publication fence is invalid".into(),
            ));
        };
        if semantic_digest.is_some_and(|expected| found_digest != expected) {
            return Ok(());
        }
        let mut keys = vec![app_key];
        let mut args = vec![previous_raw, bindings.len().to_string()];
        for entity_type in bindings.keys() {
            keys.push(Self::unscoped_generation_key(tenant, entity_type));
            keys.push(Self::unscoped_fence_key(tenant, entity_type));
            args.push("-1".into());
            args.push(String::new());
        }
        args.push(String::new());
        let result: Vec<i64> = self
            .activate_unscoped_fence_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            [1] => Ok(()),
            [-2] => Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            )),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Redis fence removal result: {other:?}"
            ))),
        }
    }

    async fn get_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
    ) -> Result<Option<StreamPublicationFence>, PersistenceError> {
        let raw: Option<String> = self
            .client
            .get(Self::unscoped_application_fence_key(tenant, application_id))
            .await
            .map_err(storage_error)?;
        raw.map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| PersistenceError::Serialization(error.to_string()))
    }

    async fn unscoped_stream_publication_fence_active(
        &self,
        tenant: &str,
        entity_type: &str,
        publication_action: &str,
        capability_digest: &str,
    ) -> Result<bool, PersistenceError> {
        let raw: Option<String> = self
            .client
            .get(Self::unscoped_fence_key(tenant, entity_type))
            .await
            .map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        Ok(value
            .get("publication_action")
            .and_then(serde_json::Value::as_str)
            == Some(publication_action)
            && value
                .get("capability_digest")
                .and_then(serde_json::Value::as_str)
                == Some(capability_digest))
    }

    async fn restore_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        expected_current_semantic_digest: &str,
        fence: &StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let StreamPublicationFence::InstalledApplication {
            application_id,
            semantic_digest,
            bindings,
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot restore global stream publications".into(),
            ));
        };
        let app_key = Self::unscoped_application_fence_key(tenant, application_id);
        let previous: Option<String> = self.client.get(&app_key).await.map_err(storage_error)?;
        let previous_fence = previous
            .as_deref()
            .map(serde_json::from_str::<StreamPublicationFence>)
            .transpose()
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        if !matches!(
            previous_fence.as_ref(),
            Some(StreamPublicationFence::InstalledApplication {
                semantic_digest,
                ..
            }) if semantic_digest == expected_current_semantic_digest
        ) {
            return Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            ));
        }
        let mut entity_types = std::collections::BTreeSet::from_iter(bindings.keys().cloned());
        if let Some(StreamPublicationFence::InstalledApplication {
            bindings: current_bindings,
            ..
        }) = previous_fence
        {
            entity_types.extend(current_bindings.into_keys());
        }
        let mut keys = vec![app_key];
        let mut args = vec![previous.unwrap_or_default(), entity_types.len().to_string()];
        for entity_type in entity_types {
            keys.push(Self::unscoped_generation_key(tenant, &entity_type));
            keys.push(Self::unscoped_fence_key(tenant, &entity_type));
            args.push("-1".into());
            if let Some(binding) = bindings.get(&entity_type) {
                args.push(
                    serde_json::to_string(&serde_json::json!({
                        "application_id": application_id,
                        "semantic_digest": semantic_digest,
                        "publication_action": binding.publication_action,
                        "capability_digest": binding.capability_digest,
                    }))
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                );
            } else {
                args.push(String::new());
            }
        }
        args.push(
            serde_json::to_string(fence)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
        );
        let result: Vec<i64> = self
            .activate_unscoped_fence_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            [1] => Ok(()),
            [-2] => Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            )),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Redis fence restore result: {other:?}"
            ))),
        }
    }

    async fn scoped_entity_bundle_digests(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        scope: &SchemaScope,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = Self::tenant_journals_key(tenant);
        let type_prefix = encode_lex_component(entity_type);
        let entity_prefix = encode_lex_component(&scoped_journal_pin_prefix(entity_id, scope));
        let member_prefix = format!("{type_prefix}!{entity_prefix}");
        const PIN_SCAN_BUDGET: usize = 256;
        let count = PIN_SCAN_BUDGET.min(i64::MAX as usize) as i64;
        let members: Vec<String> = self
            .client
            .zrangebylex(
                &key,
                format!("[{member_prefix}"),
                format!("[{member_prefix}~"),
                Some((0, count)),
            )
            .await
            .map_err(storage_error)?;
        let scan_budget_exhausted = members.len() == PIN_SCAN_BUDGET;
        let mut digests = Vec::new();
        for member in members {
            let (_, scoped_id) = Self::parse_journal_member(&member)?;
            if let Some((found_entity_id, pin)) = split_scoped_journal_entity_id(&scoped_id)
                && found_entity_id == entity_id
                && &pin.scope == scope
            {
                digests.push(pin.bundle_digest);
                if digests.len() == limit {
                    break;
                }
            }
        }
        if scan_budget_exhausted && digests.len() < limit {
            return Err(PersistenceError::Storage(
                "scoped entity pin scan budget exhausted".to_string(),
            ));
        }
        Ok(digests)
    }
    };
}
