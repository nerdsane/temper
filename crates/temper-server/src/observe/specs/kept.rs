//! Passing checks, kept by the digest of what their levels read
//! ([`temper_verify::VerificationCascade::kept_key`]).
//!
//! A spec whose levels read what an earlier passing check's levels read gets
//! that verdict back instead of running them again, so a change to text no
//! level reads, such as an error message, is not checked again. A verdict is
//! kept in the platform blob table under the kernel's own keys
//! ([`KERNEL_BLOB_PREFIX`]), which no caller's blob is written to, so a
//! restart keeps it whatever checker reached it, as a joint proof's is. A
//! server with no platform store keeps verdicts in this process. A check
//! that failed or ran out of its budget is not kept: the next one asks again.

use std::sync::PoisonError;

use temper_verify::{CascadeResult, KeptKey, VerificationCascade};

use crate::blob_store::KERNEL_BLOB_PREFIX;
use crate::state::ServerState;

fn blob_key(digest: &str) -> String {
    format!("{KERNEL_BLOB_PREFIX}check-verdict/{digest}")
}

/// The key of what `cascade`'s levels read, worked out on a blocking
/// thread: it builds the models.
pub(super) async fn key(
    cascade: impl FnOnce() -> VerificationCascade + Send + 'static,
) -> Option<KeptKey> {
    tokio::task::spawn_blocking(move || cascade().kept_key())
        .await
        .ok()
        .flatten()
}

/// The passing verdict kept under `key`, with `kept` set, if any and if it
/// stands for the spec `key` is of ([`KeptKey::fits`]).
pub(super) async fn kept(state: &ServerState, key: &KeptKey) -> Option<CascadeResult> {
    let digest = key.digest.as_str();
    let result = match state.platform_metadata_store() {
        Some(store) => match store.get_blob(&blob_key(digest)).await {
            Ok(bytes) => serde_json::from_slice(&bytes?).ok()?,
            Err(error) => {
                tracing::warn!(%digest, %error, "a kept check could not be read");
                return None;
            }
        },
        None => state
            .check_verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(digest)
            .cloned()?,
    };
    if !key.fits(&result) {
        return None;
    }
    Some(CascadeResult {
        kept: Some(digest.to_string()),
        ..result
    })
}

/// Keep `result` under `digest` when it passed every level it ran.
pub(super) async fn keep(state: &ServerState, digest: &str, result: &CascadeResult) {
    if !result.all_passed || result.stopped.is_some() {
        return;
    }
    let Some(store) = state.platform_metadata_store() else {
        state
            .check_verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest.to_string(), result.clone());
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(result)
        && let Err(error) = store.put_blob(&blob_key(digest), &bytes).await
    {
        tracing::warn!(%digest, %error, "a passing check could not be kept");
    }
}
