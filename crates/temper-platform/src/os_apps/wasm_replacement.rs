//! Whether an install replaces a WASM module someone uploaded by hand: when
//! the bundle's modules changed since the app was last installed, or the
//! upload is older than that install.

use chrono::{DateTime, NaiveDateTime, Utc};
use temper_server::state::WasmModuleSource;

use super::{AppBundle, reconcile};
use crate::state::PlatformState;

#[derive(Debug, Clone, Default)]
pub(super) struct UploadedWasmReplacementContext {
    bundle_wasm_digest_changed: bool,
    app_recorded_at: Option<DateTime<Utc>>,
}

impl UploadedWasmReplacementContext {
    pub(super) fn should_replace(&self, existing: &WasmModuleSource) -> bool {
        if existing.source != "upload" {
            return false;
        }
        if self.bundle_wasm_digest_changed {
            return true;
        }
        let Some(app_recorded_at) = self.app_recorded_at else {
            return false;
        };
        let Some(updated_at) = existing
            .updated_at
            .as_deref()
            .and_then(parse_persisted_timestamp)
        else {
            return false;
        };
        updated_at < app_recorded_at
    }
}

fn parse_persisted_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").map(|dt| dt.and_utc())
        })
        .ok()
}

pub(super) async fn uploaded_wasm_replacement_context(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> UploadedWasmReplacementContext {
    let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return UploadedWasmReplacementContext::default();
    };
    let digest = reconcile::digest_app_bundle(Some(tenant), app_name, bundle);
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(record)) => UploadedWasmReplacementContext {
            bundle_wasm_digest_changed: record.wasm_digest != digest.wasm_digest,
            app_recorded_at: record
                .last_reconciled_at
                .as_deref()
                .or(record.installed_at.as_deref())
                .and_then(parse_persisted_timestamp),
        },
        Ok(None) => UploadedWasmReplacementContext::default(),
        Err(error) => {
            tracing::warn!(
                tenant,
                app = %app_name,
                error = %error,
                "Failed to read OS app metadata while deciding WASM hot-upload replacement"
            );
            UploadedWasmReplacementContext::default()
        }
    }
}
