//! Evolution-engine durable metadata boundary.

use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{EvolutionRecordRow, FeatureRequestRow, TursoEventStore};

use super::{pg_evolution_record_to_turso, pg_feature_request_to_turso};

/// Evolution engine durable metadata capability.
#[async_trait::async_trait]
pub trait EvolutionStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError>;

    async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError>;

    async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError>;

    async fn delete_feature_request(&self, id: &str) -> Result<bool, PersistenceError>;

    async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError>;

    async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError>;

    async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;

    async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError>;
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

    async fn delete_feature_request(&self, id: &str) -> Result<bool, PersistenceError> {
        self.delete_feature_request(id).await
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

    async fn delete_feature_request(&self, id: &str) -> Result<bool, PersistenceError> {
        self.delete_feature_request(id).await
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
