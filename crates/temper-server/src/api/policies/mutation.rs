//! Canonical CAS loop for complete tenant policy publication.

use crate::authz::{PolicyPublicationError, publish_policy_snapshot};
use crate::state::ServerState;
use crate::storage::{PolicySnapshot, PolicyStoreRow};

const POLICY_PUBLICATION_RETRY_BUDGET: usize = 4;

/// Endpoint-facing policy mutation failure.
#[derive(Debug, thiserror::Error)]
pub(super) enum PolicyMutationError {
    #[error("policy not found")]
    NotFound,
    #[error("policy already exists")]
    AlreadyExists,
    #[error("{0}")]
    Invalid(String),
    #[error("policy publication remained contended after the retry budget")]
    Contended,
    #[error("{0}")]
    Unavailable(String),
}

/// Apply a logical mutation to the latest snapshot and publish it atomically.
pub(super) async fn mutate_tenant_policies<F>(
    state: &ServerState,
    tenant: &str,
    mut mutate: F,
) -> Result<PolicySnapshot, PolicyMutationError>
where
    F: FnMut(&mut Vec<PolicyStoreRow>) -> Result<(), PolicyMutationError>,
{
    let store = state.policy_store().ok_or_else(|| {
        PolicyMutationError::Unavailable("durable policy store is not configured".to_string())
    })?;
    for _attempt in 0..POLICY_PUBLICATION_RETRY_BUDGET {
        let current = store
            .load_policy_snapshot(tenant)
            .await
            .map_err(|error| PolicyMutationError::Unavailable(error.to_string()))?;
        let mut prospective = current.rows.clone();
        mutate(&mut prospective)?;
        for row in &mut prospective {
            row.tenant = tenant.to_string();
        }
        match publish_policy_snapshot(state, tenant, current.version, prospective).await {
            Ok(committed) => return Ok(committed),
            Err(PolicyPublicationError::Conflict { .. }) => continue,
            Err(PolicyPublicationError::Invalid(error)) => {
                return Err(PolicyMutationError::Invalid(error));
            }
            Err(
                PolicyPublicationError::Persistence(error)
                | PolicyPublicationError::Activation(error),
            ) => {
                return Err(PolicyMutationError::Unavailable(error));
            }
        }
    }
    Err(PolicyMutationError::Contended)
}
