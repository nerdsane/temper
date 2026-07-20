//! Design-time, OTS, blob, and authorization analytics adapters.

use super::*;

#[async_trait::async_trait]
impl DesignTimeEventStore for PostgresEventStore {
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
    ) -> Result<(), PersistenceError> {
        self.insert_design_time_event(
            kind,
            entity_type,
            tenant,
            summary,
            level,
            passed,
            step_number,
            total_steps,
        )
        .await
    }

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        self.list_design_time_events(tenant, limit)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(pg_design_time_event_to_turso)
                    .collect()
            })
    }
}

#[async_trait::async_trait]
impl DesignTimeEventStore for TursoEventStore {
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
    ) -> Result<(), PersistenceError> {
        self.insert_design_time_event(
            kind,
            entity_type,
            tenant,
            summary,
            level,
            passed,
            step_number,
            total_steps,
        )
        .await
    }

    async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        self.list_design_time_events(tenant, limit).await
    }
}

#[async_trait::async_trait]
impl OtsStore for PostgresEventStore {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_ots_trajectory(&temper_store_postgres::PostgresOtsTrajectoryParams {
            trajectory_id: params.trajectory_id,
            tenant: params.tenant,
            agent_id: params.agent_id,
            session_id: params.session_id,
            outcome: params.outcome,
            turn_count: params.turn_count,
            data: params.data,
        })
        .await
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.enqueue_ots_trajectory(&temper_store_postgres::PostgresOtsTrajectoryParams {
            trajectory_id: params.trajectory_id,
            tenant: params.tenant,
            agent_id: params.agent_id,
            session_id: params.session_id,
            outcome: params.outcome,
            turn_count: params.turn_count,
            data: params.data,
        })
        .await
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(trajectory_id).await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(trajectory_id, error).await
    }

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError> {
        self.list_queued_ots_trajectories(limit)
            .await
            .map(|rows| rows.into_iter().map(pg_queued_ots_to_turso).collect())
    }

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        self.list_ots_trajectories(tenant, agent_id, outcome, limit)
            .await
            .map(|rows| rows.into_iter().map(pg_ots_to_turso).collect())
    }

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_ots_trajectory(trajectory_id).await
    }
}

#[async_trait::async_trait]
impl OtsStore for TursoEventStore {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.persist_ots_trajectory(params).await
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.enqueue_ots_trajectory(params).await
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_persisted(trajectory_id).await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_ots_trajectory_failed(trajectory_id, error).await
    }

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<OtsQueuedTrajectoryRow>, PersistenceError> {
        self.list_queued_ots_trajectories(limit).await
    }

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        self.list_ots_trajectories(tenant, agent_id, outcome, limit)
            .await
    }

    async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get_ots_trajectory(trajectory_id).await
    }
}

#[async_trait::async_trait]
impl BlobStore for PostgresEventStore {
    async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob(key, data).await
    }

    async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, ttl).await
    }

    async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        self.sweep_expired_blobs(max_rows).await
    }

    async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_blob(key).await
    }
}

#[async_trait::async_trait]
impl BlobStore for TursoEventStore {
    async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob(key, data).await
    }

    async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, ttl).await
    }

    async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        self.sweep_expired_blobs(max_rows).await
    }

    async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_blob(key).await
    }
}

#[async_trait::async_trait]
impl AuthzAnalyticsStore for PostgresEventStore {
    async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        self.upsert_policy_denial_pattern(
            tenant,
            agent_type,
            action,
            resource_type,
            resource_id,
            timestamp,
        )
        .await
    }

    async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        self.load_policy_denial_patterns(tenant)
            .await
            .map(|rows| rows.into_iter().map(pg_denial_pattern_to_turso).collect())
    }
}

#[async_trait::async_trait]
impl AuthzAnalyticsStore for TursoEventStore {
    async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        self.upsert_policy_denial_pattern(
            tenant,
            agent_type,
            action,
            resource_type,
            resource_id,
            timestamp,
        )
        .await
    }

    async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        self.load_policy_denial_patterns(tenant).await
    }
}
