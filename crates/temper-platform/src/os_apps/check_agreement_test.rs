//! In this crate's tests, every install a request makes is first checked
//! ([`super::check_os_app`]) on the same tenant: the check must leave the
//! tenant as it was, and refuse exactly when the install refuses at its
//! checks, in the same words. So every refusal an install test covers is a
//! case of the check too, and a new install check is one of the check's by
//! construction.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use super::{InstallResult, OsAppInstallPlan, catalog, check_os_app};
use crate::genesis_install_verify::platform_store as platform_store_of;
use crate::state::PlatformState;

/// What a tenant serves and keeps: what a check must leave as it was.
#[derive(Debug, PartialEq, Eq)]
struct Tenant {
    csdl_xml: Option<String>,
    specs: BTreeMap<String, String>,
    verified: Vec<String>,
    policy: Option<String>,
    cached_policy: Option<String>,
    modules: Vec<(String, String)>,
    guardrails: bool,
    installed: Vec<String>,
    app_dir: Option<String>,
}

async fn tenant_now(state: &PlatformState, tenant: &str, app_name: &str) -> Tenant {
    let id = TenantId::new(tenant);
    let (csdl_xml, specs, verified) = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let config = registry.get_tenant(&id);
        let specs = config
            .map(|config| {
                config
                    .entities
                    .iter()
                    .map(|(name, spec)| (name.clone(), spec.ioa_source.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let verified = registry
            .verification_statuses(&id)
            .into_iter()
            .flatten()
            .map(|(name, status)| format!("{name}: {status:?}"))
            .collect();
        (
            config.map(|config| config.csdl_xml.to_string()),
            specs,
            verified,
        )
    };
    let modules = state
        .server
        .wasm_module_registry
        .read()
        .unwrap() // ci-ok: infallible lock
        .modules_for_tenant(&id)
        .into_iter()
        .map(|(name, hash)| (name.to_string(), hash.to_string()))
        .collect();
    let installed = match platform_store_of(state) {
        Some(store) => store
            .list_all_installed_apps()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(row_tenant, _)| row_tenant == tenant)
            .map(|(_, app)| app)
            .collect(),
        None => Vec::new(),
    };
    let mut installed: Vec<String> = installed;
    installed.sort();
    if let Some(store) = platform_store_of(state)
        && let Ok(Some(record)) = store.get_installed_app(tenant, app_name).await
    {
        installed.push(format!("{record:?}"));
    }
    Tenant {
        csdl_xml,
        specs,
        verified,
        policy: state.server.authz.get_tenant_policy_text(tenant),
        cached_policy: state
            .server
            .tenant_policies
            .read()
            .unwrap() // ci-ok: infallible lock
            .get(tenant)
            .cloned(),
        modules,
        guardrails: state.server.commons_guardrails_enabled(&id),
        installed,
        app_dir: catalog()
            .read()
            .unwrap() // ci-ok: infallible lock
            .app_files(Some(tenant), app_name)
            .map(|files| files.dir().display().to_string()),
    }
}

/// Check what the install of `app_name` under `plan` is about to install,
/// and that the check changed nothing. `None` when the catalog has no files
/// for it, which the install then says itself.
pub(super) async fn check_first(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
) -> Option<Result<InstallResult, String>> {
    let files = catalog()
        .read()
        .unwrap() // ci-ok: infallible lock
        .app_files(Some(tenant), app_name)
        .cloned()?;
    let before = tenant_now(state, tenant, app_name).await;
    let checked = Box::pin(check_os_app(state, tenant, app_name, &files, plan)).await;
    let after = tenant_now(state, tenant, app_name).await;
    assert_eq!(
        before, after,
        "checking '{app_name}' changed tenant '{tenant}': {checked:?}"
    );
    Some(checked)
}

tokio::task_local! {
    /// Set once an install goes past where a check stops ([`passed_the_check`]).
    static PAST_THE_CHECK: std::cell::Cell<bool>;
}

/// Mark that the install running in this task went past where a check stops:
/// from here it writes, and a failure is one a check cannot see.
pub(super) fn passed_the_check() {
    let _ = PAST_THE_CHECK.try_with(|past| past.set(true));
}

/// Run `install`, and say whether it went past where a check stops.
pub(super) async fn watched<F: std::future::Future>(install: F) -> (F::Output, bool) {
    PAST_THE_CHECK
        .scope(std::cell::Cell::new(false), async {
            let out = install.await;
            (out, PAST_THE_CHECK.with(std::cell::Cell::get))
        })
        .await
}

/// The install refuses at its checks exactly when its check did, in the same
/// words. An install may fail after its check passed only past where a check
/// stops ([`passed_the_check`]), at a write, which a check does not make.
pub(super) fn agrees(
    app_name: &str,
    checked: &Result<InstallResult, String>,
    installed: &Result<InstallResult, String>,
    past_the_check: bool,
) {
    match (checked, installed) {
        (Err(check), Err(install)) => assert_eq!(
            check, install,
            "the check of '{app_name}' and its install refused in different words"
        ),
        (Err(check), Ok(_)) => {
            panic!("the check of '{app_name}' refused what its install took: {check}")
        }
        (Ok(check), Ok(install)) => {
            assert_eq!(
                (&check.added, &check.updated, &check.skipped),
                (&install.added, &install.updated, &install.skipped),
                "the check of '{app_name}' saw other spec changes than its install"
            );
            let words = |result: &InstallResult| {
                let mut words: Vec<String> =
                    result.warnings.iter().map(|w| format!("{w:?}")).collect();
                words.sort();
                words
            };
            assert_eq!(
                words(check),
                words(install),
                "the check of '{app_name}' warned otherwise than its install"
            );
        }
        (Ok(_), Err(install)) => assert!(
            past_the_check,
            "the check of '{app_name}' passed what its install refused before any write: {install}"
        ),
    }
}
