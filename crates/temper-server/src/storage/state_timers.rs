use std::sync::Arc;

use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
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
}

forward_state_timer_store!(PostgresEventStore);
forward_state_timer_store!(TursoEventStore, keyed);
