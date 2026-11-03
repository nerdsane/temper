//! Resolve an install's named files over the tenant's catalog without registering them.

use std::path::Path;

use super::app_catalog::AppCatalog;
use super::reconcile::resolve_os_app_install_order_with_dependencies;
use super::{AppFileSource, catalog, dependencies_for_entry, os_app_dependencies};

/// Every app reconciliation will read, in dependency-first order, with the
/// same files its checks must read. Incoming apps win over the tenant's
/// catalog, just as after registration. Keep Written paths throughout: a
/// directory listing cannot silently omit a newly installed spec.
pub(crate) fn resolve_materialized_app_closure(
    tenant: &str,
    cache_root: &Path,
    apps: &[(String, AppFileSource)],
) -> Result<Vec<(String, AppFileSource)>, String> {
    let incoming = AppCatalog::from_apps(cache_root.to_path_buf(), apps.to_vec());
    let roots: Vec<String> = apps.iter().map(|(name, _)| name.clone()).collect();
    for name in &roots {
        if incoming.app_files(None, name).is_none() {
            return Err(format!("incoming install has no OS app '{name}'"));
        }
    }
    let order = resolve_os_app_install_order_with_dependencies(&roots, |name| {
        match incoming.app_entry(None, name) {
            Some(entry) => dependencies_for_entry(name, Some(entry)),
            None => os_app_dependencies(Some(tenant), name),
        }
    })?;
    order
        .into_iter()
        .map(|name| {
            let files = incoming
                .app_files(None, &name)
                .cloned()
                .or_else(|| {
                    catalog()
                        .read()
                        .expect("OS app catalog lock poisoned")
                        .app_files(Some(tenant), &name)
                        .cloned()
                })
                .ok_or_else(|| {
                    format!("OS app '{name}' not found while resolving install closure")
                })?;
            Ok((name, files))
        })
        .collect()
}
