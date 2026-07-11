//! Backend adapters for observe and evolution metadata capabilities.

use super::*;

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
        tenant: &str,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(tenant, limit)
            .await
            .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect())
    }

    async fn load_unmet_intent_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_unmet_to_turso).collect())
    }

    async fn load_submit_spec_timestamps(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps(tenant).await
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
        tenant: &str,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.load_recent_trajectories(tenant, limit).await
    }

    async fn load_unmet_intent_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        self.load_unmet_intent_rows(tenant).await
    }

    async fn load_submit_spec_timestamps(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, String>, PersistenceError> {
        self.load_submit_spec_timestamps(tenant).await
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
        tenant: &str,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            tenant,
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
        tenant: &str,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(tenant, disposition)
            .await
            .map(|rows| rows.into_iter().map(pg_feature_request_to_turso).collect())
    }

    async fn update_feature_request(
        &self,
        tenant: &str,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(tenant, id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        record: EvolutionRecordWrite<'_>,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(PostgresEvolutionRecordInsert {
            tenant: record.tenant,
            id: record.id,
            record_type: record.record_type,
            status: record.status,
            created_by: record.created_by,
            derived_from: record.derived_from,
            data_json: record.data_json,
        })
        .await
    }

    async fn get_evolution_record(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(tenant, id)
            .await
            .map(|row| row.map(pg_evolution_record_to_turso))
    }

    async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(tenant, record_type, status)
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }

    async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl EvolutionStore for TursoEventStore {
    async fn upsert_feature_request(
        &self,
        tenant: &str,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.upsert_feature_request(
            tenant,
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
        tenant: &str,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        self.list_feature_requests(tenant, disposition).await
    }

    async fn update_feature_request(
        &self,
        tenant: &str,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        self.update_feature_request(tenant, id, disposition, developer_notes)
            .await
    }

    async fn insert_evolution_record(
        &self,
        record: EvolutionRecordWrite<'_>,
    ) -> Result<(), PersistenceError> {
        self.insert_evolution_record(TursoEvolutionRecordInsert {
            tenant: record.tenant,
            id: record.id,
            record_type: record.record_type,
            status: record.status,
            created_by: record.created_by,
            derived_from: record.derived_from,
            data_json: record.data_json,
        })
        .await
    }

    async fn get_evolution_record(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        self.get_evolution_record(tenant, id).await
    }

    async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_evolution_records(tenant, record_type, status)
            .await
    }

    async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        self.list_ranked_insights(tenant).await
    }
}
