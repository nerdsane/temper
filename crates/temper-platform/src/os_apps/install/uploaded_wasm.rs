//! Source-aware replacement policy for hot-uploaded WASM modules.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::*;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(in crate::os_apps) struct UploadedWasmReplacementContext {
    bundle_wasm_digest_changed: bool,
    app_recorded_at: Option<DateTime<Utc>>,
}

impl UploadedWasmReplacementContext {
    pub(in crate::os_apps) fn publication_status(&self) -> String {
        match serde_json::to_string(self) {
            Ok(context) => format!("publishing:{context}"),
            Err(error) => {
                tracing::error!(%error, "Failed to serialize WASM publication context");
                "publishing".to_string()
            }
        }
    }

    fn from_publication_status(status: &str) -> Option<Self> {
        serde_json::from_str(status.strip_prefix("publishing:")?).ok()
    }

    pub(in crate::os_apps) fn should_replace(&self, existing: &WasmModuleSource) -> bool {
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

pub(in crate::os_apps) async fn uploaded_wasm_replacement_context(
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
    let digest = reconcile::digest_app_bundle(app_name, bundle);
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(record)) => UploadedWasmReplacementContext::from_publication_status(&record.status)
            .unwrap_or_else(|| UploadedWasmReplacementContext {
                bundle_wasm_digest_changed: record.wasm_digest != digest.wasm_digest,
                app_recorded_at: record
                    .last_reconciled_at
                    .as_deref()
                    .or(record.installed_at.as_deref())
                    .and_then(parse_persisted_timestamp),
            }),
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
