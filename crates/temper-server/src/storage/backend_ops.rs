//! Decision, WASM, data-only, and trajectory adapters.

use super::*;

#[async_trait::async_trait]
impl DecisionStore for PostgresEventStore {
    async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_decisions(tenant, status).await
    }

    async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_all_decisions(status).await
    }

    async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(id).await
    }
}

#[async_trait::async_trait]
impl DecisionStore for TursoEventStore {
    async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_decisions(tenant, status).await
    }

    async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        self.query_all_decisions(status).await
    }

    async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        self.get_pending_decision(id).await
    }
}

#[async_trait::async_trait]
impl WasmMetadataStore for PostgresEventStore {
    async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        self.load_wasm_module_metadata_all_tenants()
            .await
            .map(|rows| rows.into_iter().map(pg_wasm_metadata_to_turso).collect())
    }

    async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        self.delete_wasm_module(tenant, module_name).await
    }
}

#[async_trait::async_trait]
impl WasmMetadataStore for TursoEventStore {
    async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        self.load_wasm_module_metadata_all_tenants().await
    }

    async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        self.delete_wasm_module(tenant, module_name).await
    }
}

#[async_trait::async_trait]
impl WasmInvocationStore for PostgresEventStore {
    async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_wasm_invocation(&temper_store_postgres::PostgresWasmInvocationInsert {
            tenant: entry.tenant,
            entity_type: entry.entity_type,
            entity_id: entry.entity_id,
            module_name: entry.module_name,
            trigger_action: entry.trigger_action,
            callback_action: entry.callback_action,
            success: entry.success,
            error: entry.error,
            duration_ms: entry.duration_ms,
            created_at: entry.created_at,
        })
        .await
    }

    async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        self.load_recent_wasm_invocations(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_wasm_invocation_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl WasmInvocationStore for TursoEventStore {
    async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_wasm_invocation(entry).await
    }

    async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        self.load_recent_wasm_invocations(limit).await
    }
}

#[async_trait::async_trait]
impl DataOnlyCreateStore for PostgresEventStore {
    async fn create_data_only_entity(
        &self,
        record: DataOnlyCreateRecord<'_>,
    ) -> Result<u64, PersistenceError> {
        self.create_data_only_entity_native_with_state(
            record.tenant,
            record.entity_type,
            record.entity_id,
            record.status,
            record.fields,
            record.state,
            record.event,
        )
        .await
    }
}

fn trajectory_source_label(source: &TrajectorySource) -> &'static str {
    match source {
        TrajectorySource::Entity => "Entity",
        TrajectorySource::Platform => "Platform",
        TrajectorySource::Authz => "Authz",
    }
}

fn trajectory_request_body_json(entry: &TrajectoryEntry) -> Option<String> {
    entry.request_body.as_ref().and_then(|value| {
        let serialized = serde_json::to_string(value).ok()?;
        Some(if serialized.len() > 4096 {
            let mut end = 4096;
            while !serialized.is_char_boundary(end) {
                end -= 1;
            }
            serialized[..end].to_string()
        } else {
            serialized
        })
    })
}

fn trajectory_matched_policy_ids_json(entry: &TrajectoryEntry) -> Option<String> {
    entry
        .matched_policy_ids
        .as_ref()
        .and_then(|ids| serde_json::to_string(ids).ok())
}

#[async_trait::async_trait]
impl TrajectorySink for PostgresEventStore {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        self.persist_trajectory(PostgresTrajectoryInsert {
            tenant: &entry.tenant,
            entity_type: &entry.entity_type,
            entity_id: &entry.entity_id,
            action: &entry.action,
            success: entry.success,
            from_status: entry.from_status.as_deref(),
            to_status: entry.to_status.as_deref(),
            error: entry.error.as_deref(),
            agent_id: entry.agent_id.as_deref(),
            session_id: entry.session_id.as_deref(),
            authz_denied: entry.authz_denied,
            denied_resource: entry.denied_resource.as_deref(),
            denied_module: entry.denied_module.as_deref(),
            source,
            spec_governed: entry.spec_governed,
            created_at: &entry.timestamp,
            request_body: request_body_json.as_deref(),
            intent: entry.intent.as_deref(),
            matched_policy_ids: matched_policy_ids_json.as_deref(),
        })
        .await
        .map_err(|e| {
            format!(
                "failed to persist trajectory entry for {}/{}/{} action {} in postgres: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })
    }
}

#[async_trait::async_trait]
impl TrajectorySink for TursoEventStore {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        self.persist_trajectory(TursoTrajectoryInsert {
            tenant: &entry.tenant,
            entity_type: &entry.entity_type,
            entity_id: &entry.entity_id,
            action: &entry.action,
            success: entry.success,
            from_status: entry.from_status.as_deref(),
            to_status: entry.to_status.as_deref(),
            error: entry.error.as_deref(),
            agent_id: entry.agent_id.as_deref(),
            session_id: entry.session_id.as_deref(),
            authz_denied: entry.authz_denied,
            denied_resource: entry.denied_resource.as_deref(),
            denied_module: entry.denied_module.as_deref(),
            source,
            spec_governed: entry.spec_governed,
            created_at: &entry.timestamp,
            request_body: request_body_json.as_deref(),
            intent: entry.intent.as_deref(),
            matched_policy_ids: matched_policy_ids_json.as_deref(),
        })
        .await
        .map_err(|e| {
            format!(
                "failed to persist trajectory entry for {}/{}/{} action {} in turso: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })
    }
}

#[async_trait::async_trait]
impl TrajectorySink for TenantStoreRouter {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let store = self.store_for_tenant(&entry.tenant).await.map_err(|e| {
            format!(
                "failed to resolve tenant store for trajectory entry {}/{}/{} action {}: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })?;
        let matched_policy_ids_json = trajectory_matched_policy_ids_json(entry);
        let request_body_json = trajectory_request_body_json(entry);
        let source = entry.source.as_ref().map(trajectory_source_label);

        store
            .persist_trajectory(TursoTrajectoryInsert {
                tenant: &entry.tenant,
                entity_type: &entry.entity_type,
                entity_id: &entry.entity_id,
                action: &entry.action,
                success: entry.success,
                from_status: entry.from_status.as_deref(),
                to_status: entry.to_status.as_deref(),
                error: entry.error.as_deref(),
                agent_id: entry.agent_id.as_deref(),
                session_id: entry.session_id.as_deref(),
                authz_denied: entry.authz_denied,
                denied_resource: entry.denied_resource.as_deref(),
                denied_module: entry.denied_module.as_deref(),
                source,
                spec_governed: entry.spec_governed,
                created_at: &entry.timestamp,
                request_body: request_body_json.as_deref(),
                intent: entry.intent.as_deref(),
                matched_policy_ids: matched_policy_ids_json.as_deref(),
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to persist trajectory entry for {}/{}/{} action {} in turso-routed: {e}",
                    entry.tenant, entry.entity_type, entry.entity_id, entry.action
                )
            })
    }
}
