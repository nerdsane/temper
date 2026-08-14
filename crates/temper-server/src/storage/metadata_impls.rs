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
