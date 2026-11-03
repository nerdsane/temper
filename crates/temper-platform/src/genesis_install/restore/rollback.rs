//! Recover prior dependencies even when another install holds their files.

use super::*;
use crate::os_apps::{InstallOrigin, OsAppInstallPlan, install_os_app_with_plan};

/// Roll back the prior closure without losing recovery coordinates on partial failure.
pub(in crate::genesis_install) async fn rollback_to_prior(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
) -> Result<(), String> {
    let mut errors = Vec::new();
    // Preserve the desired versions before any fallible recovery. A failed record carries
    // recovery coordinates without claiming that the currently selected bundle is healthy.
    for record in prior_records
        .iter()
        .filter(|r| r.app_name != prior.app_name)
    {
        if let Err(error) = record_dependency_recovery(ps, record, false).await {
            errors.push(error);
        }
    }
    match ensure_prior_bundle_available(platform, ps, prior, prior_records).await {
        Ok(()) => {
            if let Err(error) = restore_prior_install(platform, ps, tenant, prior).await {
                errors.push(error);
            }
        }
        Err(error) => errors.push(error),
    }
    // Reconcile each recovered dependency even if the root or a sibling could not recover.
    // Reconcile one app at a time so a missing sibling cannot prevent independent progress.
    for record in prior_records
        .iter()
        .filter(|r| r.app_name != prior.app_name)
    {
        let matches = tenant_os_app_bundle_digest(tenant, &record.app_name).is_some_and(|digest| {
            record.bundle_digest.is_empty() || digest.bundle_digest == record.bundle_digest
        });
        let mut recovered = None;
        let ready = if matches {
            // Pending records describe the desired version, so replay its phases rather
            // than treating its saved digest as proof that the runtime is unchanged.
            match install_os_app_with_plan(
                platform,
                tenant,
                &record.app_name,
                OsAppInstallPlan::all(),
                InstallOrigin::Replay,
            )
            .await
            {
                Ok(result) => {
                    recovered = Some(result);
                    verify_install_runtime_ready(platform, ps, tenant, &record.app_name).await
                        && tenant_os_app_bundle_digest(tenant, &record.app_name).is_some_and(
                            |digest| {
                                record.bundle_digest.is_empty()
                                    || digest.bundle_digest == record.bundle_digest
                            },
                        )
                }
                Err(error) => {
                    errors.push(format!(
                        "rollback reconcile of {} failed: {error}",
                        record.app_name
                    ));
                    false
                }
            }
        } else {
            false
        };
        if !ready {
            errors.push(format!(
                "rollback dependency {} is not runtime-ready at its prior digest",
                record.app_name
            ));
        }
        // Keep the recovery record failed unless this dependency independently verified.
        if let Err(error) = record_dependency_recovery(ps, record, ready).await {
            errors.push(error);
        } else if ready && let Some(result) = recovered {
            result.publish_reviews();
        }
    }
    let app_names = prior_records
        .iter()
        .map(|r| r.app_name.clone())
        .collect::<Vec<_>>();
    if errors.is_empty()
        && !verify_install_closure_runtime_ready(platform, ps, tenant, &app_names).await
    {
        errors.push("rollback dependency closure is not runtime-ready".to_string());
    }
    for record in prior_records {
        if !record.bundle_digest.is_empty()
            && tenant_os_app_bundle_digest(tenant, &record.app_name)
                .is_none_or(|digest| digest.bundle_digest != record.bundle_digest)
        {
            errors.push(format!(
                "rollback bundle for {} does not match its prior digest",
                record.app_name
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn record_dependency_recovery(
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    ready: bool,
) -> Result<(), String> {
    let mut record = prior.clone();
    if !ready {
        record.status = "failed".to_string();
    }
    ps.record_installed_app_metadata(&record)
        .await
        .map_err(|error| {
            format!(
                "rollback could not restore dependency provenance for {}: {error}",
                prior.app_name
            )
        })
}

/// Restore every prior dependency the root cache no longer holds at its recorded digest.
pub(super) async fn restore_prior_dependencies(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
    served: &BTreeSet<String>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for record in prior_records {
        if record.app_name == prior.app_name || served.contains(&record.app_name) {
            continue;
        }
        // A dependency the install left as it was (one a push resolved, not
        // wrote) still resolves at its recorded version: nothing to restore.
        let unchanged =
            tenant_os_app_bundle_digest(&record.tenant, &record.app_name).is_some_and(|now| {
                !record.bundle_digest.is_empty() && now.bundle_digest == record.bundle_digest
            });
        if unchanged {
            continue;
        }
        if let Err(error) = restore_dependency(platform, ps, record, prior_records).await {
            errors.push(format!(
                "failed to restore prior dependency {} for rollback: {error}",
                record.app_ref
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn restore_dependency(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    record: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
) -> Result<(), String> {
    let registry_tenant = if record.registry_tenant.trim().is_empty() {
        "default"
    } else {
        record.registry_tenant.trim()
    };
    let mut candidates =
        install_cache_candidates(platform, &record.tenant, registry_tenant, &record.app_ref);
    let destination = candidates[0].path.clone();
    let own = first_matching_install(ps, &candidates, record, prior_records).await;
    let (stored, apps) = match own {
        Some(found) => found,
        None => {
            // A dependency's app_ref names its version, not the closure that cached it.
            // Only this tenant's recorded installs contribute candidate directories; every
            // candidate must still read back at the dependency's prior bundle digest.
            let mut closures = prior_records.to_vec();
            for (tenant, name) in ps.list_all_installed_apps().await? {
                if tenant != record.tenant || closures.iter().any(|r| r.app_name == name) {
                    continue;
                }
                if let Some(installed) = ps.get_installed_app(&tenant, &name).await? {
                    closures.push(installed);
                }
            }
            for closure in closures {
                if closure.tenant != record.tenant || closure.source_kind != "genesis" {
                    continue;
                }
                candidates.extend(install_cache_candidates(
                    platform,
                    &record.tenant,
                    if closure.registry_tenant.trim().is_empty() {
                        "default"
                    } else {
                        closure.registry_tenant.trim()
                    },
                    &closure.app_ref,
                ));
            }
            match first_matching_install(ps, &candidates, record, prior_records).await {
                Some(found) => found,
                None => {
                    let registry = recorded_registry(&record.registry_url)?;
                    let app_ref = parse_registry_app_ref(&record.app_ref)?;
                    materialize_registry_app_closure_via_bundle(
                        &registry,
                        registry_tenant,
                        app_ref,
                        &destination,
                    )
                    .await?;
                    legacy_specs::convert_legacy_bundle_specs(&destination);
                    let apps = cached_apps_matching_records(
                        ps,
                        &destination,
                        record,
                        prior_records,
                    )
                    .await
                    .ok_or_else(|| {
                        format!(
                            "re-materialized previous version {} does not hold its recorded bundle",
                            record.app_ref
                        )
                    })?;
                    (destination, apps)
                }
            }
        }
    };
    // Register just this dependency; siblings are restored from their own prior records.
    let own = apps.into_iter().filter(|(app, _)| app == &record.app_name);
    register_written_apps(&record.tenant, stored, own.collect());
    Ok(())
}
