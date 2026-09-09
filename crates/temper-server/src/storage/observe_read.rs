//! `ObserveReadStore` implementations for the durable backends.
//!
//! The observe read path — trajectory listings, per-session and per-agent
//! replay, aggregate stats — lives here rather than in the storage module so
//! the composite-trait module stays a declaration of the storage surface
//! instead of also carrying every backend's query bodies.

use std::collections::BTreeMap;

use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{
    AgentSummary, TursoEventStore, TursoTrajectoryRow, UnmetIntentAggRow, store::TrajectoryStats,
};

use super::{
    ObserveReadStore, pg_agent_summary_to_turso, pg_stats_to_turso, pg_trajectory_to_turso,
    pg_unmet_to_turso,
};

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
        tenant: &str,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(tenant, entity_type, action, success_filter, failed_limit)
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

    async fn query_trajectories_by_session(
        &self,
        session_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_session(session_id, tenant, entity_type, limit)
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
        tenant: &str,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        self.query_trajectory_stats(tenant, entity_type, action, success_filter, failed_limit)
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

    async fn query_trajectories_by_session(
        &self,
        session_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        self.query_trajectories_by_session(session_id, tenant, entity_type, limit)
            .await
    }

    async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        self.query_agent_summaries(tenant).await
    }
}
