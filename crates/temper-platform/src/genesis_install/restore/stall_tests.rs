//! A boot whose registry is slow for one bundle, or has lost one: the boot
//! goes on, every other install is fetched, and what failed is counted.

use std::time::Duration;

use base64::Engine as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::rollback_tests::record;
use super::*;

/// A stored Genesis install of `app` at `version` whose files are not on
/// disk, fetched again from `registry`; it answers after `delay`, or with a
/// 404 when `delay` is `None`.
async fn install_to_fetch(
    ps: &SimPlatformStore,
    registry: &MockServer,
    tenant: &str,
    app: &str,
    version: &str,
    delay: Option<Duration>,
) {
    let files = tempfile::tempdir().expect("app files");
    write_app(files.path(), app, &format!("{app} {version}"));
    let installed = InstalledAppRecord {
        registry_url: registry.uri(),
        ..record(files.path(), tenant, app, version)
    };
    ps.record_installed_app_metadata(&installed)
        .await
        .expect("record the install");
    let bundle_path = format!("/api/genesis/apps/acme/{app}/versions/{version}/bundle");
    let Some(delay) = delay else {
        Mock::given(method("GET"))
            .and(path(bundle_path))
            .respond_with(ResponseTemplate::new(404))
            .mount(registry)
            .await;
        return;
    };
    let bundle = GenesisRegistryBundleResponse {
        app_ref: installed.app_ref.clone(),
        registry_tenant: "default".to_string(),
        apps: vec![GenesisRegistryBundleApp {
            owner: "acme".to_string(),
            name: app.to_string(),
            version_hash: version.to_string(),
            files: ["app.toml", "APP.md"]
                .into_iter()
                .map(|name| GenesisRegistryBundleFile {
                    path: name.to_string(),
                    content_base64: base64::engine::general_purpose::STANDARD.encode(
                        std::fs::read(files.path().join(app).join(name)).expect("app file"),
                    ),
                })
                .collect(),
        }],
    };
    Mock::given(method("GET"))
        .and(path(bundle_path))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&bundle)
                .set_delay(delay),
        )
        .mount(registry)
        .await;
}

/// One registry bundle that does not answer for ten minutes holds neither the
/// boot nor the installs listed after it.
#[tokio::test]
async fn a_stalled_bundle_holds_neither_the_boot_nor_the_installs_after_it() {
    let registry = MockServer::start().await;
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let tenant = format!("{TENANT}-stall");
    let version = "3".repeat(40);
    install_to_fetch(
        &ps,
        &registry,
        &tenant,
        "aaa-large",
        &version,
        Some(Duration::from_secs(600)),
    )
    .await;
    for app in ["bbb-notes", "ccc-tags"] {
        install_to_fetch(&ps, &registry, &tenant, app, &version, Some(Duration::ZERO)).await;
    }
    let platform = server_on(data_dir.path(), &ps);

    tokio::time::timeout(
        Duration::from_secs(5),
        restore_genesis_registry_cache_roots(&platform),
    )
    .await
    .expect("the boot goes on while a bundle is fetched");

    let arrived = tokio::time::timeout(Duration::from_secs(10), async {
        while ["bbb-notes", "ccc-tags"]
            .iter()
            .any(|app| crate::os_apps::tenant_os_app(&tenant, app).is_none())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        arrived.is_ok(),
        "the installs after the stalled bundle are fetched while it stalls"
    );
    assert!(
        crate::os_apps::tenant_os_app(&tenant, "aaa-large").is_none(),
        "the stalled bundle is still being fetched"
    );
}

/// A bundle the registry no longer has is counted as failed, its install
/// record is kept, and the other install is fetched.
#[tokio::test]
async fn a_bundle_the_registry_lost_is_counted_and_its_record_kept() {
    let registry = MockServer::start().await;
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let tenant = format!("{TENANT}-lost");
    let version = "4".repeat(40);
    install_to_fetch(&ps, &registry, &tenant, "gone", &version, None).await;
    install_to_fetch(
        &ps,
        &registry,
        &tenant,
        "here",
        &version,
        Some(Duration::ZERO),
    )
    .await;
    let before = ps
        .get_installed_app(&tenant, "gone")
        .await
        .expect("read the record");
    let platform = server_on(data_dir.path(), &ps);

    let recovery = start_genesis_registry_cache_recovery(&platform).await;
    assert_eq!(recovery.restored, 0, "nothing was on disk");
    assert_eq!(
        recovery.fetching.finished().await,
        CacheFetchSummary {
            fetched: 1,
            failed: 1
        }
    );
    assert!(crate::os_apps::tenant_os_app(&tenant, "here").is_some());
    assert_eq!(
        ps.get_installed_app(&tenant, "gone")
            .await
            .expect("read the record"),
        before,
        "the lost install's record is kept as it was"
    );
}
