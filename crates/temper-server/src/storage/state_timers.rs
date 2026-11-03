use std::sync::Arc;

use temper_runtime::persistence::{
    OverdueTimerRows, OverdueTimers, PersistenceError, StateTimerRecord,
};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::MetadataStore;

/// Armed state timers (ADR-0056), kept so a restart arms them again.
#[async_trait::async_trait]
pub trait StateTimerStore: Send + Sync {
    /// Record an armed timer; a row armed at a higher sequence is kept.
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError>;

    /// Remove an entity's timer row if it still holds the arming `arm_id`.
    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError>;

    /// Write the due time on an entity's timer row if it still holds the
    /// arming `arm_id`, for a row an older release wrote without one. Only
    /// an update: a row its timer removed stays removed. The default keeps
    /// no rows across releases, so it has none to date.
    async fn date_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
        due_at: &str,
    ) -> Result<(), PersistenceError> {
        let _ = (tenant, entity_type, entity_id, arm_id, due_at);
        Ok(())
    }

    /// Every armed timer row for `tenant`.
    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError>;

    /// One entity's timer row, if it has one. The default reads the tenant's
    /// rows; a store with a keyed read overrides it.
    async fn state_timer_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        Ok(self
            .list_state_timers(tenant)
            .await?
            .into_iter()
            .find(|row| row.entity_type == entity_type && row.entity_id == entity_id))
    }

    /// `tenant`'s timer rows due at or before `now` (RFC 3339 in UTC): how
    /// many, and the earliest due time. The default reads the tenant's state
    /// timer rows and keeps no scheduled actions; a store with a due-time
    /// index overrides it.
    async fn overdue_timer_rows(
        &self,
        tenant: &str,
        now: &str,
    ) -> Result<OverdueTimerRows, PersistenceError> {
        let mut state_timers = OverdueTimers::default();
        for row in self.list_state_timers(tenant).await? {
            let Some(due_at) = row.due_at.filter(|due_at| due_at.as_str() <= now) else {
                continue;
            };
            state_timers.count += 1;
            if state_timers
                .oldest_due_at
                .as_ref()
                .is_none_or(|oldest| due_at < *oldest)
            {
                state_timers.oldest_due_at = Some(due_at);
            }
        }
        Ok(OverdueTimerRows {
            state_timers,
            scheduled_actions: OverdueTimers::default(),
        })
    }
}

macro_rules! forward_state_timer_store {
    ($store:ty) => {
        forward_state_timer_store!($store, {});
    };
    ($store:ty, keyed) => {
        forward_state_timer_store!($store, {
            async fn state_timer_row(
                &self,
                tenant: &str,
                entity_type: &str,
                entity_id: &str,
            ) -> Result<Option<StateTimerRecord>, PersistenceError> {
                <$store>::state_timer_row(self, tenant, entity_type, entity_id).await
            }
        });
    };
    ($store:ty, { $($keyed:tt)* }) => {
        #[async_trait::async_trait]
        impl StateTimerStore for $store {
            $($keyed)*

            async fn upsert_state_timer(
                &self,
                timer: &StateTimerRecord,
            ) -> Result<(), PersistenceError> {
                <$store>::upsert_state_timer(self, timer).await
            }

            async fn delete_state_timer(
                &self,
                tenant: &str,
                entity_type: &str,
                entity_id: &str,
                arm_id: &str,
            ) -> Result<(), PersistenceError> {
                <$store>::delete_state_timer(self, tenant, entity_type, entity_id, arm_id).await
            }

            async fn list_state_timers(
                &self,
                tenant: &str,
            ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
                <$store>::list_state_timers(self, tenant).await
            }

            async fn overdue_timer_rows(
                &self,
                tenant: &str,
                now: &str,
            ) -> Result<OverdueTimerRows, PersistenceError> {
                <$store>::overdue_timer_rows(self, tenant, now).await
            }

            async fn date_state_timer(
                &self,
                tenant: &str,
                entity_type: &str,
                entity_id: &str,
                arm_id: &str,
                due_at: &str,
            ) -> Result<(), PersistenceError> {
                <$store>::date_state_timer(self, tenant, entity_type, entity_id, arm_id, due_at)
                    .await
            }
        }
    };
}

/// A tenant's metadata store, as the store of its timer rows.
pub struct MetadataTimerRows(pub Arc<dyn MetadataStore>);

#[async_trait::async_trait]
impl StateTimerStore for MetadataTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(timer).await
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id)
            .await
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        self.0.list_state_timers(tenant).await
    }

    async fn state_timer_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        self.0.state_timer_row(tenant, entity_type, entity_id).await
    }

    async fn overdue_timer_rows(
        &self,
        tenant: &str,
        now: &str,
    ) -> Result<OverdueTimerRows, PersistenceError> {
        self.0.overdue_timer_rows(tenant, now).await
    }

    async fn date_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
        due_at: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .date_state_timer(tenant, entity_type, entity_id, arm_id, due_at)
            .await
    }
}

forward_state_timer_store!(PostgresEventStore);
forward_state_timer_store!(TursoEventStore, keyed);
