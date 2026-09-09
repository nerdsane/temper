use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;

use super::{CommonsStorageCapError, CommonsStorageCapExceeded, CommonsStorageProjection};
use crate::state::ServerState;

const MAX_PENDING_STORAGE_RESERVATIONS: usize = 4096;

#[derive(Debug, Clone)]
pub(crate) struct CommonsStorageReservationEntry {
    tenant: String,
    owner_id: String,
    bytes: i64,
}

/// RAII reservation for owner-attributed raw Blob bytes.
///
/// Dropping it removes the pending bytes on success, failure, or cancelled
/// request futures. Persisted Blob metadata remains the durable source used by
/// the next storage projection.
pub(crate) struct CommonsStorageReservation {
    id: String,
    reservations: Arc<Mutex<BTreeMap<String, CommonsStorageReservationEntry>>>,
}

impl Drop for CommonsStorageReservation {
    fn drop(&mut self) {
        if let Ok(mut reservations) = self.reservations.lock() {
            reservations.remove(&self.id);
        }
    }
}

impl ServerState {
    /// Reserve the exact owner-attributed bytes for an admitted raw Blob.
    ///
    /// The caller must hold [`Self::acquire_commons_write_guardrail_lock`]
    /// while creating the reservation so the durable projection and pending
    /// ledger form one admission snapshot. The returned guard can then live
    /// across streaming I/O without holding the coarse mutation lock.
    pub(crate) async fn reserve_commons_blob_storage(
        &self,
        tenant: &TenantId,
        blob_id: &str,
        repository_id: &str,
        size_bytes: i64,
    ) -> Result<Option<CommonsStorageReservation>, CommonsStorageCapError> {
        if !self.commons_guardrails_enabled(tenant)
            || !self.storage_cap_entities_available(tenant)?
            || size_bytes <= 0
        {
            return Ok(None);
        }
        if self.blob_already_exists(tenant, blob_id).await {
            return Ok(None);
        }

        let owner_id = self
            .repository_owner_id(tenant, repository_id)
            .await?
            .ok_or_else(|| {
                CommonsStorageCapError::MissingAttribution(format!(
                    "Repository '{repository_id}' is required for commons storage attribution"
                ))
            })?;
        let Some(profile) = self.owner_storage_profile(tenant, &owner_id).await? else {
            return Ok(None);
        };
        if profile.suspended {
            return Err(CommonsStorageCapError::OwnerSuspended(profile.owner_id));
        }
        let projection = self
            .commons_storage_projection_for_owner(tenant, &owner_id)
            .await?
            .unwrap_or(CommonsStorageProjection {
                owner_id: owner_id.clone(),
                used_bytes: 0,
                cap_bytes: profile.cap_bytes,
            });
        let reserved_bytes = self.pending_reserved_bytes(tenant, &owner_id)?;
        let effective_used = projection.used_bytes.saturating_add(reserved_bytes);
        if effective_used.saturating_add(size_bytes) > projection.cap_bytes {
            return Err(CommonsStorageCapError::Exceeded(
                CommonsStorageCapExceeded {
                    owner_id,
                    used_bytes: effective_used,
                    additional_bytes: size_bytes,
                    cap_bytes: projection.cap_bytes,
                },
            ));
        }

        let reservation_id = sim_uuid().to_string();
        let mut reservations = self.commons_storage_reservations.lock().map_err(|error| {
            CommonsStorageCapError::Internal(format!("storage reservation lock poisoned: {error}"))
        })?;
        if reservations.len() >= MAX_PENDING_STORAGE_RESERVATIONS {
            return Err(CommonsStorageCapError::ReservationCapacityExhausted);
        }
        let previous = reservations.insert(
            reservation_id.clone(),
            CommonsStorageReservationEntry {
                tenant: tenant.to_string(),
                owner_id,
                bytes: size_bytes,
            },
        );
        debug_assert!(
            previous.is_none(),
            "sim_uuid reservation IDs must be unique"
        );
        drop(reservations);
        Ok(Some(CommonsStorageReservation {
            id: reservation_id,
            reservations: self.commons_storage_reservations.clone(),
        }))
    }

    pub(super) fn pending_reserved_bytes(
        &self,
        tenant: &TenantId,
        owner_id: &str,
    ) -> Result<i64, CommonsStorageCapError> {
        let reservations = self.commons_storage_reservations.lock().map_err(|error| {
            CommonsStorageCapError::Internal(format!("storage reservation lock poisoned: {error}"))
        })?;
        Ok(reservations
            .values()
            .filter(|reservation| {
                reservation.tenant == tenant.as_str() && reservation.owner_id == owner_id
            })
            .fold(0i64, |total, reservation| {
                total.saturating_add(reservation.bytes.max(0))
            }))
    }
}
