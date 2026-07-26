//! Durable metadata capability traits for the evolution engine and
//! design-time verification events, implemented by the storage backends.

use temper_runtime::persistence::PersistenceError;
use temper_store_turso::{
    DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow, OtsQueuedTrajectoryRow,
    OtsTrajectoryParams, OtsTrajectoryRow,
};

/// Evolution engine durable metadata capability.
#[async_trait::async_trait]
pub trait EvolutionStore: Send + Sync {
    /// Insert a generated feature request, or refresh the generator-owned
    /// fields of an existing one. Developer-owned fields (disposition,
    /// developer notes) are written ONLY on first insert (ARN-240). Returns
    /// `true` when this call created the record.
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
    ) -> Result<bool, PersistenceError>;

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

/// Design-time verification event capability.
#[async_trait::async_trait]
pub trait DesignTimeEventStore: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn insert_design_time_event(
        &self,
        kind: &str,
        entity_type: &str,
        tenant: &str,
        summary: &str,
        level: Option<&str>,
        passed: Option<bool>,
        step_number: Option<i64>,
        total_steps: Option<i64>,
    ) -> Result<(), PersistenceError>;

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError>;
}

/// OTS trajectory capability.
#[async_trait::async_trait]
pub trait OtsStore: Send + Sync {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError>;

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError>;

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError>;

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError>;

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError>;

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError>;

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError>;
}
