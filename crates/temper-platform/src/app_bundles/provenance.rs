use std::path::Path;

use temper_server::platform_store::InstalledAppRecord;

use super::types::InstallBundleRequest;
use crate::os_apps::{digest_app_bundle_with_version, load_app_bundle};
use crate::state::PlatformState;

pub(super) async fn record_local_provenance(
    platform: &PlatformState,
    request: &InstallBundleRequest,
    view: &Path,
    apps: &[String],
) -> Result<(), String> {
    let Some(store) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Err("local bundle installation requires durable platform storage".to_string());
    };
    for app_name in apps {
        let app = request
            .manifest
            .apps
            .iter()
            .find(|app| &app.name == app_name)
            .ok_or_else(|| format!("canonical app '{app_name}' is absent"))?;
        let app_dir = view.join(app_name);
        let bundle = load_app_bundle(&app_dir)
            .ok_or_else(|| format!("canonical app '{app_name}' failed to reload"))?;
        let app_guide = std::fs::read_to_string(app_dir.join("APP.md")).ok();
        let digest =
            digest_app_bundle_with_version(app_name, &app.version, app_guide.as_deref(), &bundle);
        let record = InstalledAppRecord {
            tenant: request.tenant.clone(),
            app_name: app_name.clone(),
            source_kind: "local_bundle".to_string(),
            app_ref: request.provenance.source_locator.clone(),
            version_hash: request.manifest.bundle_digest.clone(),
            pinned_version_hash: request.manifest.bundle_digest.clone(),
            current_version_hash: request.manifest.bundle_digest.clone(),
            follow_policy: "pinned".to_string(),
            closure_id: format!("bundle:{}", request.manifest.bundle_digest),
            registry_url: String::new(),
            registry_tenant: String::new(),
            dependency_lock_digest: request.provenance.lock_digest.clone(),
            app_version: digest.app_version,
            bundle_digest: digest.bundle_digest,
            spec_digest: digest.spec_digest,
            policy_digest: digest.policy_digest,
            wasm_digest: digest.wasm_digest,
            content_digest: digest.content_digest,
            seed_digest: digest.seed_digest,
            installed_at: None,
            last_reconciled_at: None,
            status: "installed".to_string(),
        };
        store
            .record_installed_app_metadata(&record)
            .await
            .map_err(|error| format!("persist local provenance for '{}': {error}", app_name))?;
    }
    Ok(())
}
