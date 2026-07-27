//! Bounded, deterministic Redis entity discovery indexes.

use std::collections::BTreeMap;

use fred::prelude::*;
use fred::types::scan::Scanner;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tokio_stream::StreamExt;

use super::{EntityRef, RedisEventStore};

impl RedisEventStore {
    pub(super) fn tenant_ordered_entities_key(tenant: &str) -> String {
        format!("{}:entities_ordered:{tenant}", crate::keys::PREFIX)
    }

    pub(super) fn tenant_ordered_type_entities_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:entities_ordered:{tenant}:{entity_type}",
            crate::keys::PREFIX
        )
    }

    fn tenant_ordered_entities_migrated_key(tenant: &str) -> String {
        format!("{}:entities_ordered_migrated:{tenant}", crate::keys::PREFIX)
    }

    async fn ensure_ordered_entity_index(&self, tenant: &str) -> Result<(), PersistenceError> {
        let marker_key = Self::tenant_ordered_entities_migrated_key(tenant);
        let migrated: bool = self
            .client
            .exists(&marker_key)
            .await
            .map_err(storage_error)?;
        if migrated {
            let legacy_count: i64 = self
                .client
                .scard(Self::tenant_entities_key(tenant))
                .await
                .map_err(storage_error)?;
            let ordered_count: i64 = self
                .client
                .zcard(Self::tenant_ordered_entities_key(tenant))
                .await
                .map_err(storage_error)?;
            if legacy_count == ordered_count {
                return Ok(());
            }
        }

        // Migrate the legacy SET incrementally. Each SSCAN page is decoded and
        // inserted into both ordered indexes before requesting the next page, so
        // client memory stays bounded. New appends dual-write all indexes in one
        // Lua script, making concurrent migration idempotent and race-safe.
        const MIGRATION_PAGE_SIZE: u32 = 256;
        let mut pages = Box::pin(self.client.sscan(
            Self::tenant_entities_key(tenant),
            "*",
            Some(MIGRATION_PAGE_SIZE),
        ));
        while let Some(page) = pages.next().await {
            let mut page = page.map_err(storage_error)?;
            let mut global = Vec::new();
            let mut by_type = BTreeMap::<String, Vec<(f64, String)>>::new();
            for value in page.take_results().unwrap_or_default() {
                let encoded: String = value.convert().map_err(storage_error)?;
                let entity_ref: EntityRef = serde_json::from_str(&encoded)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                global.push((0.0, encoded.clone()));
                by_type
                    .entry(entity_ref.entity_type)
                    .or_default()
                    .push((0.0, encoded));
            }
            if !global.is_empty() {
                let _: i64 = self
                    .client
                    .zadd(
                        Self::tenant_ordered_entities_key(tenant),
                        None,
                        None,
                        false,
                        false,
                        global,
                    )
                    .await
                    .map_err(storage_error)?;
            }
            for (entity_type, members) in by_type {
                let _: i64 = self
                    .client
                    .zadd(
                        Self::tenant_ordered_type_entities_key(tenant, &entity_type),
                        None,
                        None,
                        false,
                        false,
                        members,
                    )
                    .await
                    .map_err(storage_error)?;
            }
            page.next();
        }
        let _: () = self
            .client
            .set(&marker_key, "1", None, None, false)
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub(super) async fn list_entity_ids_limited_ordered(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.ensure_ordered_entity_index(tenant).await?;

        let ordered_key = entity_type.map_or_else(
            || Self::tenant_ordered_entities_key(tenant),
            |entity_type| Self::tenant_ordered_type_entities_key(tenant, entity_type),
        );
        let end = limit.saturating_sub(1).min(i64::MAX as usize) as i64;
        let encoded: Vec<String> = self
            .client
            .zrange(ordered_key, 0, end, None, false, None, false)
            .await
            .map_err(storage_error)?;
        let mut out = Vec::with_capacity(encoded.len());
        for encoded in encoded {
            let entity_ref: EntityRef = serde_json::from_str(&encoded)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            if entity_type.is_some_and(|expected| expected != entity_ref.entity_type) {
                return Err(PersistenceError::Storage(
                    "Redis typed entity index contains a different entity type".to_string(),
                ));
            }
            out.push((entity_ref.entity_type, entity_ref.entity_id));
        }
        Ok(out)
    }
}
