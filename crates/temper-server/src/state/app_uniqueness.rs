use serde_json::Value;
use temper_runtime::tenant::TenantId;

use super::ServerState;

const APP_ENTITY_TYPE: &str = "App";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommonsAppNameConflict {
    pub(crate) owner_id: String,
    pub(crate) name: String,
    pub(crate) existing_app_id: String,
}

#[derive(Debug, Clone)]
pub(crate) enum CommonsAppUniquenessError {
    Conflict(CommonsAppNameConflict),
    Internal(String),
}

impl std::fmt::Display for CommonsAppUniquenessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => write!(
                f,
                "app name '{}/{}' is already registered by App '{}'",
                conflict.owner_id, conflict.name, conflict.existing_app_id
            ),
            Self::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CommonsAppUniquenessError {}

impl ServerState {
    pub(crate) async fn enforce_commons_app_name_unique_for_write(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: &Value,
    ) -> Result<(), CommonsAppUniquenessError> {
        if !self.commons_guardrails_enabled(tenant) || entity_type != APP_ENTITY_TYPE {
            return Ok(());
        }
        if !self
            .is_entity_type_governed(tenant, APP_ENTITY_TYPE)
            .map_err(CommonsAppUniquenessError::Internal)?
        {
            return Ok(());
        }

        let Some(owner_id) = first_non_empty_string(fields, &["OwnerId", "ChildOwnerId"]) else {
            return Ok(());
        };
        let Some(name) = first_non_empty_string(fields, &["Name", "ChildName"]) else {
            return Ok(());
        };
        let owner_key = normalize_key(&owner_id);
        let name_key = normalize_key(&name);

        for candidate_id in self.list_entity_ids_lazy(tenant, APP_ENTITY_TYPE).await {
            if candidate_id == entity_id {
                continue;
            }
            if !self
                .ensure_entity_loaded(tenant, APP_ENTITY_TYPE, &candidate_id)
                .await
            {
                continue;
            }
            let candidate = self
                .get_tenant_entity_state(tenant, APP_ENTITY_TYPE, &candidate_id)
                .await
                .map_err(|e| {
                    CommonsAppUniquenessError::Internal(format!(
                        "failed to read App '{candidate_id}' for commons uniqueness: {e}"
                    ))
                })?;
            let candidate_owner = first_non_empty_string(&candidate.state.fields, &["OwnerId"]);
            let candidate_name = first_non_empty_string(&candidate.state.fields, &["Name"]);
            if candidate_owner.as_deref().map(normalize_key) == Some(owner_key.clone())
                && candidate_name.as_deref().map(normalize_key) == Some(name_key.clone())
            {
                return Err(CommonsAppUniquenessError::Conflict(
                    CommonsAppNameConflict {
                        owner_id,
                        name,
                        existing_app_id: candidate_id,
                    },
                ));
            }
        }

        Ok(())
    }
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn first_non_empty_string(fields: &Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = fields.get(*name).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}
