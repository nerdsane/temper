use serde_json::Value;
use temper_runtime::tenant::TenantId;

use super::ServerState;

const OWNER_ENTITY_TYPE: &str = "Owner";
const RATE_LIMIT_ENTITY_TYPE: &str = "RateLimit";

#[derive(Debug, Clone)]
pub(crate) struct CommonsAccountVerificationRequired {
    pub(crate) owner_id: String,
    pub(crate) status: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum CommonsAccountVerificationError {
    Required(CommonsAccountVerificationRequired),
    OwnerSuspended(String),
    MissingOwner(String),
    Internal(String),
}

impl std::fmt::Display for CommonsAccountVerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommonsAccountVerificationError::Required(required) => {
                if let Some(status) = required.status.as_deref() {
                    write!(
                        f,
                        "owner '{}' must be verified before writing to the commons (status: {status})",
                        required.owner_id
                    )
                } else {
                    write!(
                        f,
                        "owner '{}' must be verified before writing to the commons",
                        required.owner_id
                    )
                }
            }
            CommonsAccountVerificationError::OwnerSuspended(owner_id) => {
                write!(f, "owner '{owner_id}' is suspended")
            }
            CommonsAccountVerificationError::MissingOwner(owner_id) => {
                write!(
                    f,
                    "owner '{owner_id}' must exist and be verified before writing to the commons"
                )
            }
            CommonsAccountVerificationError::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CommonsAccountVerificationError {}

#[derive(Debug, Clone)]
struct OwnerVerificationProfile {
    owner_id: String,
    status: String,
    verified: bool,
    suspended: bool,
}

impl ServerState {
    pub(crate) async fn enforce_commons_verified_owner_for_write(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        fields: &Value,
    ) -> Result<(), CommonsAccountVerificationError> {
        if !self.commons_account_verification_applies(tenant, entity_type)? {
            return Ok(());
        }

        let Some(owner_id) = self.owner_id_for_commons_fields(tenant, fields).await? else {
            return Ok(());
        };
        self.require_commons_verified_owner(tenant, &owner_id).await
    }

    pub(crate) async fn enforce_commons_verified_owner_for_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        current_fields: &Value,
        params: &Value,
    ) -> Result<(), CommonsAccountVerificationError> {
        if !self.commons_account_verification_applies(tenant, entity_type)? {
            return Ok(());
        }

        let owner_id = self
            .owner_id_for_commons_fields(tenant, params)
            .await?
            .or(self
                .owner_id_for_commons_fields(tenant, current_fields)
                .await?);
        if let Some(owner_id) = owner_id {
            self.require_commons_verified_owner(tenant, &owner_id)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn require_commons_verified_owner(
        &self,
        tenant: &TenantId,
        owner_id: &str,
    ) -> Result<(), CommonsAccountVerificationError> {
        let Some(profile) = self.owner_verification_profile(tenant, owner_id).await? else {
            return Err(CommonsAccountVerificationError::MissingOwner(
                owner_id.to_string(),
            ));
        };

        if profile.suspended {
            return Err(CommonsAccountVerificationError::OwnerSuspended(
                profile.owner_id,
            ));
        }
        if !profile.verified {
            return Err(CommonsAccountVerificationError::Required(
                CommonsAccountVerificationRequired {
                    owner_id: profile.owner_id,
                    status: Some(profile.status),
                },
            ));
        }
        Ok(())
    }

    fn commons_account_verification_applies(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, CommonsAccountVerificationError> {
        if !self.commons_guardrails_enabled(tenant)
            || account_verification_exempt_entity(entity_type)
        {
            return Ok(false);
        }

        self.is_entity_type_governed(tenant, OWNER_ENTITY_TYPE)
            .map_err(CommonsAccountVerificationError::Internal)
    }

    async fn owner_id_for_commons_fields(
        &self,
        tenant: &TenantId,
        fields: &Value,
    ) -> Result<Option<String>, CommonsAccountVerificationError> {
        if let Some(owner_id) = first_non_empty_string(
            fields,
            &[
                "ChildOwnerId",
                "OwnerId",
                "OwnerAccountId",
                "AccountId",
                "CreatedBy",
                "owner_id",
                "ownerAccountId",
                "accountId",
            ],
        ) {
            return Ok(Some(owner_id));
        }

        if let Some(repository_id) = first_non_empty_string(
            fields,
            &[
                "RepositoryId",
                "ChildRepositoryId",
                "repository_id",
                "repositoryId",
            ],
        ) {
            return self
                .commons_repository_owner_id(tenant, &repository_id)
                .await;
        }

        Ok(None)
    }

    async fn commons_repository_owner_id(
        &self,
        tenant: &TenantId,
        repository_id: &str,
    ) -> Result<Option<String>, CommonsAccountVerificationError> {
        let repository = self
            .get_tenant_entity_state_authoritative(tenant, "Repository", repository_id)
            .await
            .map_err(|e| {
                CommonsAccountVerificationError::Internal(format!(
                    "failed to read Repository '{repository_id}' for account verification: {e}"
                ))
            })?;
        let Some(repository) = repository else {
            return Ok(None);
        };

        Ok(first_non_empty_string(
            &repository.state.fields,
            &["OwnerAccountId", "OwnerId", "AccountId"],
        ))
    }

    async fn owner_verification_profile(
        &self,
        tenant: &TenantId,
        owner_id: &str,
    ) -> Result<Option<OwnerVerificationProfile>, CommonsAccountVerificationError> {
        if let Some(profile) = self
            .get_tenant_entity_state_authoritative(tenant, OWNER_ENTITY_TYPE, owner_id)
            .await
            .map_err(|error| {
                CommonsAccountVerificationError::Internal(format!(
                    "failed to read Owner '{owner_id}' for account verification: {error}"
                ))
            })?
        {
            return Ok(Some(OwnerVerificationProfile::from_fields(
                owner_id,
                &profile.state.status,
                &profile.state.fields,
            )));
        }

        for candidate_id in self
            .list_entity_ids_lazy(tenant, OWNER_ENTITY_TYPE)
            .await
            .map_err(|error| {
                CommonsAccountVerificationError::Internal(format!(
                    "failed to list Owners for account verification: {error}"
                ))
            })?
        {
            let owner = self
                .get_tenant_entity_state_authoritative(tenant, OWNER_ENTITY_TYPE, &candidate_id)
                .await
                .map_err(|e| {
                    CommonsAccountVerificationError::Internal(format!(
                        "failed to read Owner '{candidate_id}' for account verification: {e}"
                    ))
                })?;
            let Some(owner) = owner else {
                continue;
            };
            let matches_account = first_non_empty_string(&owner.state.fields, &["AccountId"])
                .as_deref()
                == Some(owner_id);
            let matches_id = first_non_empty_string(&owner.state.fields, &["Id"])
                .as_deref()
                .unwrap_or(candidate_id.as_str())
                == owner_id;
            if matches_account || matches_id {
                return Ok(Some(OwnerVerificationProfile::from_fields(
                    &candidate_id,
                    &owner.state.status,
                    &owner.state.fields,
                )));
            }
        }

        Ok(None)
    }
}

impl OwnerVerificationProfile {
    fn from_fields(entity_id: &str, current_status: &str, fields: &Value) -> Self {
        let owner_id = first_non_empty_string(fields, &["AccountId", "Id"])
            .unwrap_or_else(|| entity_id.to_string());
        let status = first_non_empty_string(fields, &["Status"])
            .unwrap_or_else(|| current_status.to_string());
        let verification_surface_present = matches!(
            status.as_str(),
            "PendingVerification" | "Verified" | "Suspended"
        ) || first_non_empty_string(
            fields,
            &[
                "VerificationStatus",
                "VerificationProvider",
                "VerificationSubject",
                "VerifiedAt",
            ],
        )
        .is_some();
        let verification_status = first_non_empty_string(fields, &["VerificationStatus"])
            .map(|value| value.to_ascii_lowercase());
        let verified_at_present = first_non_empty_string(fields, &["VerifiedAt"]).is_some();
        let verified = if verification_surface_present {
            status == "Verified"
                || verification_status.as_deref() == Some("verified")
                || verified_at_present
        } else {
            status == "Active"
        };
        let suspended = status == "Suspended";

        Self {
            owner_id,
            status,
            verified,
            suspended,
        }
    }
}

fn account_verification_exempt_entity(entity_type: &str) -> bool {
    matches!(entity_type, OWNER_ENTITY_TYPE | RATE_LIMIT_ENTITY_TYPE)
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
