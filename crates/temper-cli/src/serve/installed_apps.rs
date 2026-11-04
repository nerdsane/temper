//! Startup phase 8b: replay the app installs this server stored, and apply
//! the `--app` requests.

use std::collections::{BTreeMap, BTreeSet};

use temper_platform::genesis_install;
use temper_platform::state::PlatformState;

use super::bootstrap::skip_unavailable_tenant;
use super::later::Later;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppBootstrapSource {
    Persisted,
    Cli,
}

/// What boot prints for an `--app` it has no files for: the specs it stored
/// for the app still serve, or, with none stored, it serves nothing for the
/// app and waits for its first install.
pub(super) fn missing_app_line(app: &str, tenant: &str, stored: bool) -> String {
    if stored {
        temper_platform::os_apps::stored_line(app, tenant)
    } else {
        temper_platform::os_apps::waiting_line(app, tenant)
    }
}

/// Phase 8b: Restore persisted local apps and apply explicit `--app` requests.
///
/// Why this exists:
/// - agent bootstrap (Phase 8) can replace tenant specs;
/// - App installs are durably tracked in `tenant_installed_apps`.
///
/// This phase replays persisted installs so app entities remain available
/// after restart, and then applies explicit CLI installs for `default`.
///
/// Only the server's own tenants' installs are replayed now; each install of
/// a tenant `later` defers waits there for that tenant's restore, with the
/// cache roots of the service-side installs into it.
pub(super) async fn bootstrap_installed_apps(
    state: &PlatformState,
    apps: &[String],
    later: &mut Later,
) -> anyhow::Result<()> {
    let mut requested: BTreeMap<(String, String), AppBootstrapSource> = BTreeMap::new();

    if let Some(platform_store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        match platform_store.list_all_installed_apps().await {
            Ok(installed) => {
                for (tenant, app_name) in installed {
                    if later.defers(state, &tenant) {
                        later.tenant(&tenant).installs.push(app_name);
                        continue;
                    }
                    requested.insert((tenant, app_name), AppBootstrapSource::Persisted);
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to load installed apps: {e}");
            }
        }
    }

    // Apps whose install this server stored: their specs come back from its
    // database at boot, so it serves them even with no files for them anywhere.
    let stored: BTreeSet<(String, String)> = requested.keys().cloned().collect();

    for app_name in apps {
        requested
            .entry(("default".to_string(), app_name.clone()))
            .and_modify(|source| *source = AppBootstrapSource::Cli)
            .or_insert(AppBootstrapSource::Cli);
    }

    if let Some(manifest_path) = std::env::var_os("TEMPER_BOOTSTRAP_MANIFEST")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        let tenant = std::env::var("TEMPER_BOOTSTRAP_TENANT")
            .ok()
            .filter(|tenant| !tenant.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        if !skip_unavailable_tenant(state, &tenant, "the bootstrap closure") {
            let result = temper_platform::os_apps::bootstrap_closure_manifest(
                state,
                &tenant,
                &manifest_path,
            )
            .await
            .map_err(anyhow::Error::msg)?;
            println!(
                "  Bootstrap closure '{}' installed for '{}': {}",
                result.closure.id,
                tenant,
                result.installed_apps.join(", ")
            );
        }
    }

    let mut own_roots = Vec::new();
    for (target, roots) in genesis_install::plan_app_cache_roots(state).await {
        if later.defers(state, &target) {
            later.tenant(&target).cache_roots.extend(roots);
        } else {
            own_roots.extend(roots);
        }
    }
    let restored_genesis_caches =
        genesis_install::restore_planned_cache_roots(state, own_roots).await;
    let restored_genesis_registry_caches =
        genesis_install::start_registry_cache_recovery_of(state, stored.iter().cloned().collect())
            .await
            .restored;
    if restored_genesis_caches > 0 || restored_genesis_registry_caches > 0 {
        println!(
            "  Restored Genesis app cache roots: service={restored_genesis_caches}, registry={restored_genesis_registry_caches}"
        );
    }
    replay(state, requested, &stored).await;
    Ok(())
}

/// Replay a boot's recorded installs and apply its `--app` requests, each of
/// `requested` with where it came from. `stored` are the installs this
/// server recorded.
async fn replay(
    state: &PlatformState,
    requested: BTreeMap<(String, String), AppBootstrapSource>,
    stored: &BTreeSet<(String, String)>,
) {
    // A replay of an unchanged install only registers what the boot loaded,
    // whose schema is the union of every stored row's: rebuild it from the
    // installed apps first, as their installs did.
    let installed: Vec<(String, String)> = stored.iter().cloned().collect();
    temper_platform::os_apps::converge_installed_schemas(state, &installed).await;
    for ((tenant, app_name), source) in requested {
        if skip_unavailable_tenant(state, &tenant, &format!("app '{app_name}'")) {
            continue;
        }
        if matches!(source, AppBootstrapSource::Persisted)
            && temper_platform::os_apps::tenant_os_app(&tenant, &app_name).is_none()
        {
            use std::io::Write as _;

            let mut stderr = std::io::stderr();
            let _ = writeln!(
                stderr,
                "  Info: skipping persisted app '{app_name}' for '{tenant}': not present in configured app sources"
            );
            continue;
        }
        // An app asked for by `--app` with no files anywhere yet has nothing to
        // install until its first push (`POST /api/os-apps/{name}/install`);
        // from that push on, its pushed version is what resolves here.
        if matches!(source, AppBootstrapSource::Cli)
            && temper_platform::os_apps::tenant_os_app(&tenant, &app_name).is_none()
        {
            let was_stored = stored.contains(&(tenant.clone(), app_name.clone()));
            println!("{}", missing_app_line(&app_name, &tenant, was_stored));
            continue;
        }

        // Replay through the real installer every startup. Presence-only checks
        // are not sufficient because an installed app's bundle can drift:
        // updated IOA/Cedar/WASM/ADR assets must refresh even when the tenant
        // already has entity tables from an older install. A replay of a stored
        // install keeps its recorded origin; only an `--app` request is a new
        // install. An `--app` whose stored install is a pushed version replays
        // it too: the restore above made that version what the app resolves,
        // and a new install would record it as the catalog's files.
        let install = match source {
            AppBootstrapSource::Persisted => {
                temper_platform::restore_os_app(state, &tenant, &app_name).await
            }
            AppBootstrapSource::Cli
                if temper_platform::genesis_install::pushed_install_recorded(
                    state, &tenant, &app_name,
                )
                .await =>
            {
                temper_platform::restore_os_app(state, &tenant, &app_name).await
            }
            AppBootstrapSource::Cli => {
                temper_platform::install_os_app(state, &tenant, &app_name).await
            }
        };
        match install {
            Ok(result) => match source {
                AppBootstrapSource::Persisted => {
                    let all: Vec<String> = result
                        .added
                        .iter()
                        .chain(&result.updated)
                        .chain(&result.skipped)
                        .cloned()
                        .collect();
                    println!(
                        "  Restored app '{app_name}' for '{tenant}': {}",
                        all.join(", ")
                    );
                }
                AppBootstrapSource::Cli => {
                    // A start's supervisor reads the notes from these lines, so they wait for Jev.
                    let result = result.with_reviews_read().await;
                    if !result.warnings.is_empty() || !result.modules_reviewed.is_empty() {
                        print_install_notes(
                            &app_name,
                            &serde_json::json!({
                                "warnings": result.warnings,
                                "modules_reviewed": result.modules_reviewed,
                            }),
                        );
                    }
                    println!(
                        "{}",
                        temper_platform::os_apps::installed_line(&app_name, &tenant, &result)
                    );
                    // The version just served from the boot catalog becomes the
                    // app's first pushed version when it has none, so every
                    // later boot serves it until the next push, whatever the
                    // catalog holds then (see `adopt_catalog_app`).
                    match temper_platform::genesis_install::adopt_catalog_app(
                        state, &tenant, &app_name,
                    )
                    .await
                    {
                        Ok(Some(digest)) => tracing::info!(
                            tenant = %tenant,
                            app = %app_name,
                            %digest,
                            "adopted the boot catalog's version as the first pushed version"
                        ),
                        Ok(None) => {}
                        Err(error) => tracing::error!(
                            tenant = %tenant,
                            app = %app_name,
                            %error,
                            "the served version was not adopted; the next boot serves the catalog's"
                        ),
                    }
                }
            },
            Err(e) => {
                if matches!(source, AppBootstrapSource::Cli) {
                    print_install_notes(&app_name, &serde_json::json!({ "refused": e }));
                }
                // A stored install that does not come back is an outage of that
                // app, so it is logged as an error; a `--app` request only warns.
                let message = match source {
                    AppBootstrapSource::Persisted => {
                        tracing::error!(
                            tenant = %tenant,
                            app = %app_name,
                            error = %e,
                            "Stored app install was not restored"
                        );
                        format!("Error: stored app '{app_name}' for '{tenant}' was not restored")
                    }
                    AppBootstrapSource::Cli => {
                        format!("Warning: failed to install app '{app_name}' for '{tenant}'")
                    }
                };
                eprintln!("  {message}: {e}");
            }
        }
    }
}

/// Replay `tenant`'s recorded installs of `apps`, for a tenant restored after
/// the server serves, as a boot replays its own tenants' installs.
pub(super) async fn replay_recorded_installs(
    state: &PlatformState,
    tenant: &str,
    apps: Vec<String>,
) {
    let requested: BTreeMap<_, _> = apps
        .into_iter()
        .map(|app| ((tenant.to_string(), app), AppBootstrapSource::Persisted))
        .collect();
    let stored = requested.keys().cloned().collect();
    replay(state, requested, &stored).await;
}

/// Everything the install of an app given on the command line said, as one compact JSON object on one line before the
/// line that says it installed or failed: its warnings and the modules Jev reviewed, or why it was refused, whole
/// (`refused`). A reader of a start's output that sees the verdict already has it (Katagami's start wrapper keeps both
/// lines and passes this one on as it is).
fn print_install_notes(app_name: &str, line: &serde_json::Value) {
    use std::io::Write as _;

    let _ = writeln!(
        std::io::stdout(),
        "  App '{app_name}' install notes: {line}"
    );
}
