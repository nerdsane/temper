//! Aggregate-bounded field-overflow hydration.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use temper_runtime::tenant::TenantId;

use super::{field_overflow_descriptor, field_overflow_sha256};
#[cfg(test)]
use crate::blob_store::BlobStore;
use crate::blob_store::{BlobReadBounded, hex_lower};
use crate::state::ServerState;

const GENERIC_INLINE_HYDRATION_BUDGET_BYTES: usize = 1024 * 1024;
const GENERIC_MAX_INLINE_FIELD_BYTES: usize = 128 * 1024;
const WASM_DEFERRED_BLOB_BUDGET_BYTES: usize = 16 * 1024 * 1024;
const WASM_MAX_DEFERRED_BLOB_BYTES: usize = 8 * 1024 * 1024;
const MAX_BLOB_REFS_PER_VALUE: usize = 1024;
const MAX_BLOB_READ_ATTEMPTS_PER_RESPONSE: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct BlobHydrationBudget {
    inner: Arc<Mutex<BlobHydrationBudgetState>>,
    max_inline_field_bytes: usize,
    max_deferred_field_bytes: usize,
}

#[derive(Debug)]
struct BlobHydrationBudgetState {
    inline_remaining: usize,
    deferred_remaining: usize,
    read_attempts_remaining: usize,
    failed_keys: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug)]
enum HydrationBudgetKind {
    Inline,
    Deferred,
}

#[derive(Debug)]
struct HydrationReservation {
    budget: BlobHydrationBudget,
    kind: HydrationBudgetKind,
    reserved: usize,
    committed: bool,
}

impl BlobHydrationBudget {
    pub(crate) fn generic_response() -> Self {
        Self::new(
            GENERIC_INLINE_HYDRATION_BUDGET_BYTES,
            GENERIC_MAX_INLINE_FIELD_BYTES,
            0,
            0,
        )
    }

    pub(crate) fn wasm_dispatch() -> Self {
        Self::new(
            GENERIC_INLINE_HYDRATION_BUDGET_BYTES,
            GENERIC_MAX_INLINE_FIELD_BYTES,
            WASM_DEFERRED_BLOB_BUDGET_BYTES,
            WASM_MAX_DEFERRED_BLOB_BYTES,
        )
    }

    pub(crate) fn new(
        inline_bytes: usize,
        max_inline_field_bytes: usize,
        deferred_bytes: usize,
        max_deferred_field_bytes: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BlobHydrationBudgetState {
                inline_remaining: inline_bytes,
                deferred_remaining: deferred_bytes,
                read_attempts_remaining: MAX_BLOB_READ_ATTEMPTS_PER_RESPONSE,
                failed_keys: BTreeSet::new(),
            })),
            max_inline_field_bytes,
            max_deferred_field_bytes,
        }
    }

    fn try_reserve_inline(&self, declared_size: usize) -> Option<HydrationReservation> {
        self.try_reserve(
            HydrationBudgetKind::Inline,
            declared_size,
            self.max_inline_field_bytes,
        )
    }

    fn try_reserve_deferred(&self, declared_size: usize) -> Option<HydrationReservation> {
        self.try_reserve(
            HydrationBudgetKind::Deferred,
            declared_size,
            self.max_deferred_field_bytes,
        )
    }

    fn try_reserve(
        &self,
        kind: HydrationBudgetKind,
        requested: usize,
        max_field_bytes: usize,
    ) -> Option<HydrationReservation> {
        if requested == 0 || requested > max_field_bytes {
            return None;
        }
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let remaining = match kind {
            HydrationBudgetKind::Inline => &mut state.inline_remaining,
            HydrationBudgetKind::Deferred => &mut state.deferred_remaining,
        };
        if requested > *remaining {
            return None;
        }
        *remaining -= requested;
        Some(HydrationReservation {
            budget: self.clone(),
            kind,
            reserved: requested,
            committed: false,
        })
    }

    #[cfg(test)]
    pub(super) fn remaining(&self) -> (usize, usize) {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        (state.inline_remaining, state.deferred_remaining)
    }

    fn refund(&self, kind: HydrationBudgetKind, bytes: usize) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let remaining = match kind {
            HydrationBudgetKind::Inline => &mut state.inline_remaining,
            HydrationBudgetKind::Deferred => &mut state.deferred_remaining,
        };
        *remaining = remaining.saturating_add(bytes);
    }

    fn try_begin_read(&self) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if state.read_attempts_remaining == 0 {
            return false;
        }
        state.read_attempts_remaining -= 1;
        true
    }

    fn is_known_failed(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failed_keys
            .contains(key)
    }

    fn mark_failed(&self, key: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failed_keys
            .insert(key.to_string());
    }

    #[cfg(test)]
    pub(crate) fn read_attempts_remaining(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .read_attempts_remaining
    }
}

impl HydrationReservation {
    fn max_bytes(&self) -> usize {
        self.reserved
    }

    fn commit(mut self, actual_bytes: usize) {
        debug_assert!(actual_bytes <= self.reserved);
        let refund = self.reserved.saturating_sub(actual_bytes);
        if refund > 0 {
            self.budget.refund(self.kind, refund);
        }
        self.committed = true;
    }
}

impl Drop for HydrationReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.budget.refund(self.kind, self.reserved);
        }
    }
}

fn collect_blob_ref_pointers(value: &Value, pointer: &str, out: &mut Vec<String>) {
    if out.len() >= MAX_BLOB_REFS_PER_VALUE {
        return;
    }
    if field_overflow_descriptor(value).is_some() {
        out.push(pointer.to_string());
        return;
    }

    match value {
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                collect_blob_ref_pointers(child, &format!("{pointer}/{index}"), out);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                collect_blob_ref_pointers(child, &format!("{pointer}/{escaped}"), out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(super) enum BlobReadSource<'a> {
    #[cfg(test)]
    Store(&'a BlobStore),
    Tenant {
        state: &'a ServerState,
        tenant: &'a TenantId,
    },
}

async fn read_blob_ref_bytes(
    source: &BlobReadSource<'_>,
    key: &str,
    max_bytes: usize,
) -> Result<BlobReadBounded, String> {
    match source {
        #[cfg(test)]
        BlobReadSource::Store(store) => store.get_bounded(key, max_bytes).await,
        BlobReadSource::Tenant { state, tenant } => {
            state
                .get_blob_with_legacy_fallback_bounded(tenant, key, max_bytes)
                .await
        }
    }
}

fn blob_bytes_match_key(key: &str, bytes: &[u8]) -> bool {
    let Some(expected) = field_overflow_sha256(key) else {
        return false;
    };
    hex_lower(&Sha256::digest(bytes)) == expected
}

#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value(store: &BlobStore, value: &mut Value) {
    let budget = BlobHydrationBudget::new(16 * 1024 * 1024, 16 * 1024 * 1024, 0, 0);
    let _deferred =
        hydrate_blob_refs_with_source(&BlobReadSource::Store(store), value, &budget).await;
}

/// Hydrate refs below `max_inline_bytes`; return larger values for WASM streaming.
#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value_with_ceiling(
    store: &BlobStore,
    value: &mut Value,
    max_inline_bytes: usize,
) -> BTreeMap<String, Vec<u8>> {
    let budget = BlobHydrationBudget::new(
        max_inline_bytes,
        max_inline_bytes,
        16 * 1024 * 1024,
        16 * 1024 * 1024,
    );
    hydrate_blob_refs_with_source(&BlobReadSource::Store(store), value, &budget).await
}

pub(super) async fn hydrate_blob_refs_with_source(
    source: &BlobReadSource<'_>,
    value: &mut Value,
    budget: &BlobHydrationBudget,
) -> BTreeMap<String, Vec<u8>> {
    let mut deferred_blobs = BTreeMap::new();
    let mut pointers = Vec::new();
    collect_blob_ref_pointers(value, "", &mut pointers);
    pointers.sort();

    for pointer in pointers {
        let Some((key, declared_size)) = (|| {
            let slot = if pointer.is_empty() {
                Some(&*value)
            } else {
                value.pointer(&pointer)
            }?;
            let descriptor = field_overflow_descriptor(slot)?;
            let size = usize::try_from(descriptor.serialized_bytes).ok()?;
            Some((descriptor.key.to_owned(), size))
        })() else {
            continue;
        };
        if budget.is_known_failed(&key) {
            continue;
        }

        if let Some(reservation) = budget.try_reserve_inline(declared_size) {
            if !budget.try_begin_read() {
                continue;
            }
            match read_blob_ref_bytes(source, &key, reservation.max_bytes()).await {
                Ok(BlobReadBounded::Found(bytes)) => {
                    if bytes.len() != declared_size {
                        tracing::warn!(%key, actual_bytes = bytes.len(), declared_size, "field-overflow blob length did not match its descriptor");
                        budget.mark_failed(&key);
                        continue;
                    }
                    if !blob_bytes_match_key(&key, &bytes) {
                        tracing::warn!(%key, "field-overflow blob failed SHA-256 verification");
                        budget.mark_failed(&key);
                        continue;
                    }
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(restored) => {
                            reservation.commit(bytes.len());
                            if pointer.is_empty() {
                                *value = restored;
                            } else if let Some(slot) = value.pointer_mut(&pointer) {
                                *slot = restored;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%key, %error, "failed to decode hydrated field-overflow blob");
                            budget.mark_failed(&key);
                        }
                    }
                }
                Ok(BlobReadBounded::Missing) => {
                    tracing::warn!(%key, "field-overflow blob missing during hydration");
                    budget.mark_failed(&key);
                }
                Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
                    tracing::warn!(%key, ?actual_bytes, "field-overflow blob exceeded inline hydration reservation");
                    budget.mark_failed(&key);
                }
                Err(error) => {
                    tracing::warn!(%key, %error, "failed to hydrate field-overflow blob");
                    budget.mark_failed(&key);
                }
            }
            continue;
        }

        if deferred_blobs.contains_key(&key) {
            continue;
        }
        let Some(reservation) = budget.try_reserve_deferred(declared_size) else {
            continue;
        };
        if !budget.try_begin_read() {
            continue;
        }
        match read_blob_ref_bytes(source, &key, reservation.max_bytes()).await {
            Ok(BlobReadBounded::Found(bytes)) => {
                if bytes.len() != declared_size {
                    tracing::warn!(%key, actual_bytes = bytes.len(), declared_size, "deferred field-overflow blob length did not match its descriptor");
                    budget.mark_failed(&key);
                    continue;
                }
                if !blob_bytes_match_key(&key, &bytes) {
                    tracing::warn!(%key, "deferred field-overflow blob failed SHA-256 verification");
                    budget.mark_failed(&key);
                    continue;
                }
                reservation.commit(bytes.len());
                deferred_blobs.insert(key, bytes);
            }
            Ok(BlobReadBounded::Missing) => {
                tracing::warn!(%key, "deferred field-overflow blob missing");
                budget.mark_failed(&key);
            }
            Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
                tracing::warn!(%key, ?actual_bytes, "deferred field-overflow blob exceeded cache reservation");
                budget.mark_failed(&key);
            }
            Err(error) => {
                tracing::warn!(%key, %error, "failed to fetch deferred field-overflow blob");
                budget.mark_failed(&key);
            }
        }
    }

    deferred_blobs
}

pub(crate) async fn hydrate_blob_refs_for_tenant(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
) {
    let budget = BlobHydrationBudget::generic_response();
    let _deferred = hydrate_blob_refs_for_tenant_with_budget(state, tenant, value, &budget).await;
}

pub(crate) async fn hydrate_blob_refs_for_tenant_with_budget(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
    budget: &BlobHydrationBudget,
) -> BTreeMap<String, Vec<u8>> {
    hydrate_blob_refs_with_source(&BlobReadSource::Tenant { state, tenant }, value, budget).await
}
