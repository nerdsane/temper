//! Policy, naming, observation, and evolution store impls.

use super::convert::*;
use super::*;

#[async_trait::async_trait]
impl PolicyStore for PostgresEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TursoEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TenantStoreRouter {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        let mut rows: Vec<PolicyStoreRow> = self
            .platform_store()
            .load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())?;
        for tenant_id in self.connected_tenants().await {
            if let Ok(store) = self.store_for_tenant(&tenant_id).await {
                let mut tenant_rows: Vec<PolicyStoreRow> = store
                    .load_all_policies()
                    .await
                    .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                    .map_err(|e| e.to_string())?;
                rows.append(&mut tenant_rows);
            }
        }
        Ok(rows)
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|e| e.to_string())?;
        store
            .delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

impl BackendNamedStore for PostgresEventStore {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }
}

impl BackendNamedStore for TursoEventStore {
    fn backend_name(&self) -> &'static str {
        "turso"
    }
}

#[async_trait::async_trait]
impl ObserveReadStore for PostgresEventStore {
    async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect())
    }

    async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows()
            .await
            .map(|rows| rows.into_iter().map(pg_unmet_to_turso).collect())
    }

    async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps().await
    }

    async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        self.count_trajectories_by_tenant().await
    }

    async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(entity_type, action, success_filter, failed_limit)
            .await
            .map(pg_stats_to_turso)
    }

    async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
            .await
            .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect())
    }

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        self.query_agent_summaries(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_agent_summary_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl ObserveReadStore for TursoEventStore {
    async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(limit).await
    }

    async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows().await
    }

    async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps().await
    }

    async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        self.count_trajectories_by_tenant().await
    }

    async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(entity_type, action, success_filter, failed_limit)
            .await
    }

    async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
            .await
    }

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        self.query_agent_summaries(tenant).await
    }
}

#[async_trait::async_trait]
impl EvolutionStore for PostgresEventStore {
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            id,
            category,
            description,
            frequency,
            trajectory_refs_json,
            disposition,
            developer_notes,
        )
        .await
    }

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(disposition)
            .await
            .map(|rows| rows.into_iter().map(pg_feature_request_to_turso).collect())
    }

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(id, record_type, status, created_by, derived_from, data_json)
            .await
    }

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(id)
            .await
            .map(|row| row.map(pg_evolution_record_to_turso))
    }

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(record_type, status)
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights()
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl EvolutionStore for TursoEventStore {
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            id,
            category,
            description,
            frequency,
            trajectory_refs_json,
            disposition,
            developer_notes,
        )
        .await
    }

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(disposition).await
    }

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(id, record_type, status, created_by, derived_from, data_json)
            .await
    }

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(id).await
    }

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(record_type, status).await
    }

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights().await
    }
}
