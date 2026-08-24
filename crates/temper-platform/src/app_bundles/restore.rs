use std::collections::BTreeSet;

use crate::state::PlatformState;

/// Restore Genesis installs that already have a canonical bundle cache pin.
pub async fn restore_canonical_genesis_bundle_cache_roots(
    platform: &PlatformState,
) -> Result<usize, String> {
    let _cache_guard = super::bundle_cache_lock().lock().await;
    let Some(store) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(0);
    };
    let installed = store.list_all_installed_apps().await?;
    let mut restored = 0usize;
    let mut roots = BTreeSet::new();
    for (tenant, app_name) in installed {
        let Some(root_record) = store.get_installed_app(&tenant, &app_name).await? else {
            continue;
        };
        let Some(digest) = root_record
            .closure_id
            .strip_prefix("bundle:")
            .filter(|_| root_record.source_kind == "genesis")
        else {
            continue;
        };
        if !roots.insert((tenant.clone(), digest.to_string())) {
            continue;
        }
        let manifest = super::cache::read_cached_manifest(&platform.server.data_dir, digest)?;
        let view = super::cache::materialize_view(&platform.server.data_dir, &manifest)?;
        super::verify::verify_materialized_bundle(&view, &manifest)?;
        let mut provenance = Vec::new();
        for app in &manifest.apps {
            if let Some(record) = store.get_installed_app(&tenant, &app.name).await?
                && record.source_kind == "genesis"
                && record.closure_id == root_record.closure_id
            {
                provenance.push(record);
            }
        }
        super::cache::reconcile_materialized_bundle(platform, &tenant, &manifest, &view).await?;
        for record in provenance {
            store.record_installed_app_metadata(&record).await?;
        }
        restored += 1;
    }
    Ok(restored)
}
