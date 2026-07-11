use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_runtime::tenant::TenantId;

use super::ServerState;

const OWNER_ENTITY_TYPE: &str = "Owner";
const REPOSITORY_ENTITY_TYPE: &str = "Repository";
const BLOB_ENTITY_TYPE: &str = "Blob";

#[derive(Debug, Clone)]
pub(crate) struct CommonsStorageWrite {
    pub(crate) entity_type: String,
    pub(crate) entity_id: String,
    pub(crate) action: String,
    pub(crate) fields: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct CommonsStorageProjection {
    pub(crate) owner_id: String,
    pub(crate) used_bytes: i64,
    pub(crate) cap_bytes: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct CommonsStorageCapExceeded {
    pub(crate) owner_id: String,
    pub(crate) used_bytes: i64,
    pub(crate) additional_bytes: i64,
    pub(crate) cap_bytes: i64,
}

#[derive(Debug, Clone)]
pub(crate) enum CommonsStorageCapError {
    Exceeded(CommonsStorageCapExceeded),
    OwnerSuspended(String),
    MissingAttribution(String),
    Internal(String),
}

impl std::fmt::Display for CommonsStorageCapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommonsStorageCapError::Exceeded(exceeded) => write!(
                f,
                "storage cap exceeded for owner '{}': used {} + new {} > cap {}",
                exceeded.owner_id,
                exceeded.used_bytes,
                exceeded.additional_bytes,
                exceeded.cap_bytes
            ),
            CommonsStorageCapError::OwnerSuspended(owner_id) => {
                write!(f, "owner '{owner_id}' is suspended")
            }
            CommonsStorageCapError::MissingAttribution(msg) => f.write_str(msg),
            CommonsStorageCapError::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CommonsStorageCapError {}

#[derive(Debug, Clone)]
struct OwnerStorageProfile {
    owner_id: String,
    cap_bytes: i64,
    suspended: bool,
}

impl ServerState {
    pub(crate) async fn acquire_commons_write_guardrail_lock(
        &self,
        tenant: &TenantId,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if self.commons_guardrails_enabled(tenant) {
            Some(self.commons_write_guardrail_lock.clone().lock_owned().await)
        } else {
            None
        }
    }

    pub(crate) fn clear_commons_storage_projection_cache(&self) {
        if let Ok(mut projections) = self.commons_storage_projection_cache.lock() {
            projections.clear();
        }
    }

    pub(crate) fn clear_commons_storage_projection_cache_for_entity(&self, entity_type: &str) {
        if storage_projection_entity(entity_type) {
            self.clear_commons_storage_projection_cache();
        }
    }

    pub(crate) async fn enforce_commons_storage_cap_for_write(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        fields: &Value,
    ) -> Result<(), CommonsStorageCapError> {
        self.enforce_commons_storage_caps_for_writes(
            tenant,
            &[CommonsStorageWrite {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                fields: fields.clone(),
            }],
        )
        .await
    }

    pub(crate) async fn enforce_commons_storage_caps_for_writes(
        &self,
        tenant: &TenantId,
        writes: &[CommonsStorageWrite],
    ) -> Result<(), CommonsStorageCapError> {
        if !self.commons_guardrails_enabled(tenant)
            || !self.storage_cap_entities_available(tenant)?
        {
            return Ok(());
        }

        let mut pending_repositories = BTreeMap::new();
        for write in writes {
            if write.entity_type == REPOSITORY_ENTITY_TYPE
                && is_create_action(&write.action)
                && let Some(owner_id) = repository_owner_from_fields(&write.fields)
            {
                pending_repositories.insert(write.entity_id.clone(), owner_id);
            }
        }

        let existing_blob_ids = self.existing_blob_create_ids(tenant, writes).await?;
        let mut repository_owner_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut touched_owners = BTreeSet::new();
        let mut additional_by_owner: BTreeMap<String, i64> = BTreeMap::new();
        for write in writes {
            if write.entity_type == REPOSITORY_ENTITY_TYPE && is_create_action(&write.action) {
                if let Some(owner_id) = repository_owner_from_fields(&write.fields) {
                    touched_owners.insert(owner_id);
                }
                continue;
            }

            if write.entity_type != BLOB_ENTITY_TYPE || !is_create_action(&write.action) {
                continue;
            }

            if existing_blob_ids.contains(&write.entity_id) {
                continue;
            }

            let size = read_i64(&write.fields, "Size").unwrap_or(0).max(0);
            if size == 0 {
                continue;
            }

            let repository_id = read_string(&write.fields, "RepositoryId").ok_or_else(|| {
                CommonsStorageCapError::MissingAttribution(
                    "Blob.Create requires RepositoryId for commons storage attribution".to_string(),
                )
            })?;

            let owner_id = if let Some(owner_id) = pending_repositories.get(&repository_id) {
                owner_id.clone()
            } else {
                let owner = if let Some(owner) = repository_owner_cache.get(&repository_id) {
                    owner.clone()
                } else {
                    let owner = self.repository_owner_id(tenant, &repository_id).await?;
                    repository_owner_cache.insert(repository_id.clone(), owner.clone());
                    owner
                };
                owner.ok_or_else(|| {
                    CommonsStorageCapError::MissingAttribution(format!(
                        "Repository '{repository_id}' is required for commons storage attribution"
                    ))
                })?
            };

            touched_owners.insert(owner_id.clone());
            *additional_by_owner.entry(owner_id).or_default() += size;
        }

        for owner_id in touched_owners {
            let Some(profile) = self.owner_storage_profile(tenant, &owner_id).await? else {
                continue;
            };
            if profile.suspended {
                return Err(CommonsStorageCapError::OwnerSuspended(profile.owner_id));
            }
            let projection = self
                .commons_storage_projection_for_owner(tenant, &profile.owner_id)
                .await?
                .unwrap_or(CommonsStorageProjection {
                    owner_id: profile.owner_id.clone(),
                    used_bytes: 0,
                    cap_bytes: profile.cap_bytes,
                });
            let additional_bytes = additional_by_owner
                .get(&profile.owner_id)
                .copied()
                .unwrap_or(0);
            if projection.used_bytes.saturating_add(additional_bytes) > projection.cap_bytes {
                return Err(CommonsStorageCapError::Exceeded(
                    CommonsStorageCapExceeded {
                        owner_id: projection.owner_id,
                        used_bytes: projection.used_bytes,
                        additional_bytes,
                        cap_bytes: projection.cap_bytes,
                    },
                ));
            }
        }

        Ok(())
    }

    async fn existing_blob_create_ids(
        &self,
        tenant: &TenantId,
        writes: &[CommonsStorageWrite],
    ) -> Result<BTreeSet<String>, CommonsStorageCapError> {
        let mut existing = BTreeSet::new();
        let mut candidates = BTreeSet::new();
        for write in writes {
            if write.entity_type != BLOB_ENTITY_TYPE || !is_create_action(&write.action) {
                continue;
            }
            candidates.insert(write.entity_id.clone());
        }

        for blob_id in candidates {
            if self.blob_already_exists(tenant, &blob_id).await? {
                existing.insert(blob_id);
            }
        }
        Ok(existing)
    }

    pub(crate) async fn commons_storage_projection_for_owner(
        &self,
        tenant: &TenantId,
        owner_id: &str,
    ) -> Result<Option<CommonsStorageProjection>, CommonsStorageCapError> {
        let Some(profile) = self.owner_storage_profile(tenant, owner_id).await? else {
            return Ok(None);
        };

        let cache_key = storage_projection_cache_key(tenant, &profile.owner_id);
        if let Some(cached) = self
            .commons_storage_projection_cache
            .lock()
            .map_err(|e| {
                CommonsStorageCapError::Internal(format!("storage projection lock poisoned: {e}"))
            })?
            .get(&cache_key)
            .cloned()
        {
            return Ok(Some(cached));
        }

        let mut repo_owner_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut used_bytes = 0i64;
        for blob_id in self
            .list_entity_ids_lazy(tenant, BLOB_ENTITY_TYPE)
            .await
            .map_err(|error| {
                CommonsStorageCapError::Internal(format!(
                    "failed to list Blobs for storage projection: {error}"
                ))
            })?
        {
            let blob = self
                .get_tenant_entity_state_authoritative(tenant, BLOB_ENTITY_TYPE, &blob_id)
                .await
                .map_err(|e| {
                    CommonsStorageCapError::Internal(format!(
                        "failed to read Blob '{blob_id}' for storage projection: {e}"
                    ))
                })?;
            let Some(blob) = blob else {
                continue;
            };
            let Some(repository_id) = read_string(&blob.state.fields, "RepositoryId") else {
                continue;
            };
            let repo_owner = if let Some(owner) = repo_owner_cache.get(&repository_id) {
                owner.clone()
            } else {
                let owner = self.repository_owner_id(tenant, &repository_id).await?;
                repo_owner_cache.insert(repository_id.clone(), owner.clone());
                owner
            };
            if repo_owner.as_deref() == Some(&profile.owner_id) {
                used_bytes = used_bytes
                    .saturating_add(read_i64(&blob.state.fields, "Size").unwrap_or(0).max(0));
            }
        }

        let projection = CommonsStorageProjection {
            owner_id: profile.owner_id,
            used_bytes,
            cap_bytes: profile.cap_bytes,
        };
        self.commons_storage_projection_cache
            .lock()
            .map_err(|e| {
                CommonsStorageCapError::Internal(format!("storage projection lock poisoned: {e}"))
            })?
            .insert(cache_key, projection.clone());

        Ok(Some(projection))
    }

    fn storage_cap_entities_available(
        &self,
        tenant: &TenantId,
    ) -> Result<bool, CommonsStorageCapError> {
        let owner = self
            .is_entity_type_governed(tenant, OWNER_ENTITY_TYPE)
            .map_err(CommonsStorageCapError::Internal)?;
        let repository = self
            .is_entity_type_governed(tenant, REPOSITORY_ENTITY_TYPE)
            .map_err(CommonsStorageCapError::Internal)?;
        let blob = self
            .is_entity_type_governed(tenant, BLOB_ENTITY_TYPE)
            .map_err(CommonsStorageCapError::Internal)?;
        Ok(owner && repository && blob)
    }

    async fn blob_already_exists(
        &self,
        tenant: &TenantId,
        blob_id: &str,
    ) -> Result<bool, CommonsStorageCapError> {
        self.get_tenant_entity_state_authoritative(tenant, BLOB_ENTITY_TYPE, blob_id)
            .await
            .map(|entity| entity.is_some())
            .map_err(|error| {
                CommonsStorageCapError::Internal(format!(
                    "failed to verify Blob '{blob_id}' existence: {error}"
                ))
            })
    }

    async fn repository_owner_id(
        &self,
        tenant: &TenantId,
        repository_id: &str,
    ) -> Result<Option<String>, CommonsStorageCapError> {
        let repository = self
            .get_tenant_entity_state_authoritative(tenant, REPOSITORY_ENTITY_TYPE, repository_id)
            .await
            .map_err(|e| {
                CommonsStorageCapError::Internal(format!(
                    "failed to read Repository '{repository_id}': {e}"
                ))
            })?;
        let Some(repository) = repository else {
            return Ok(None);
        };

        Ok(repository_owner_from_fields(&repository.state.fields))
    }

    async fn owner_storage_profile(
        &self,
        tenant: &TenantId,
        owner_id: &str,
    ) -> Result<Option<OwnerStorageProfile>, CommonsStorageCapError> {
        if let Some(owner) = self
            .get_tenant_entity_state_authoritative(tenant, OWNER_ENTITY_TYPE, owner_id)
            .await
            .map_err(|error| {
                CommonsStorageCapError::Internal(format!(
                    "failed to read Owner '{owner_id}': {error}"
                ))
            })?
        {
            return OwnerStorageProfile::from_fields(owner_id, &owner.state.fields).map(Some);
        }

        for candidate_id in self
            .list_entity_ids_lazy(tenant, OWNER_ENTITY_TYPE)
            .await
            .map_err(|error| {
                CommonsStorageCapError::Internal(format!(
                    "failed to list Owners for storage projection: {error}"
                ))
            })?
        {
            let owner = self
                .get_tenant_entity_state_authoritative(tenant, OWNER_ENTITY_TYPE, &candidate_id)
                .await
                .map_err(|e| {
                    CommonsStorageCapError::Internal(format!(
                        "failed to read Owner '{candidate_id}': {e}"
                    ))
                })?;
            let Some(owner) = owner else {
                continue;
            };
            let matches_account =
                read_string(&owner.state.fields, "AccountId").as_deref() == Some(owner_id);
            let matches_id = read_string(&owner.state.fields, "Id")
                .as_deref()
                .unwrap_or(candidate_id.as_str())
                == owner_id;
            if matches_account || matches_id {
                return OwnerStorageProfile::from_fields(&candidate_id, &owner.state.fields)
                    .map(Some);
            }
        }

        Ok(None)
    }
}

impl OwnerStorageProfile {
    fn from_fields(entity_id: &str, fields: &Value) -> Result<Self, CommonsStorageCapError> {
        let owner_id = read_string(fields, "AccountId")
            .or_else(|| read_string(fields, "Id"))
            .unwrap_or_else(|| entity_id.to_string());
        let Some(cap_bytes) = read_i64(fields, "StorageCapBytes") else {
            return Ok(Self {
                owner_id,
                cap_bytes: i64::MAX,
                suspended: false,
            });
        };
        let suspended = fields
            .get("Status")
            .and_then(|v| v.as_str())
            .is_some_and(|status| status == "Suspended");
        Ok(Self {
            owner_id,
            cap_bytes: cap_bytes.max(0),
            suspended,
        })
    }
}

fn is_create_action(action: &str) -> bool {
    action == "Create"
}

fn storage_projection_entity(entity_type: &str) -> bool {
    matches!(
        entity_type,
        OWNER_ENTITY_TYPE | REPOSITORY_ENTITY_TYPE | BLOB_ENTITY_TYPE
    )
}

fn storage_projection_cache_key(tenant: &TenantId, owner_id: &str) -> String {
    format!("{tenant}:{owner_id}")
}

fn repository_owner_from_fields(fields: &Value) -> Option<String> {
    read_string(fields, "OwnerAccountId")
        .or_else(|| read_string(fields, "OwnerId"))
        .or_else(|| read_string(fields, "AccountId"))
}

fn read_string(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn read_i64(fields: &Value, key: &str) -> Option<i64> {
    fields.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
    })
}
