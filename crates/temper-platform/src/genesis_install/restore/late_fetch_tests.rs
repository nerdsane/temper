//! Controlled registry arrivals exercise recovery against real installs and replay.

use std::time::Duration;

use axum::{Json, Router, routing::get};
use base64::Engine as _;
use tokio::sync::Notify;

use super::rollback_tests::{cache, record};
use super::*;

#[derive(Clone, Default)]
struct Arrival {
    requested: Arc<Notify>,
    release: Arc<Notify>,
}

struct Registry {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Registry {
    async fn serve(bundles: Vec<GenesisRegistryBundleResponse>) -> (Self, Vec<Arrival>) {
        let mut routes = Router::new();
        let mut arrivals = Vec::new();
        for bundle in bundles {
            let root = parse_registry_app_ref(&bundle.app_ref).expect("bundle ref");
            let route = format!(
                "/api/genesis/apps/{}/{}/versions/{}/bundle",
                root.owner,
                root.name,
                root.version_hash.expect("pinned version")
            );
            let arrival = Arrival::default();
            arrivals.push(arrival.clone());
            routes = routes.route(
                &route,
                get(move || {
                    let bundle = bundle.clone();
                    let arrival = arrival.clone();
                    async move {
                        arrival.requested.notify_one();
                        arrival.release.notified().await;
                        Json(bundle)
                    }
                }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("registry listener");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("registry address")
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, routes)
                .await
                .expect("registry server");
        });
        (Self { url, task }, arrivals)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn bundle(root: &Path, app: &str, version: &str) -> GenesisRegistryBundleResponse {
    let apps = written::read(root).expect("bundle file list");
    GenesisRegistryBundleResponse {
        app_ref: format!("acme/{app}@{version}"),
        registry_tenant: "default".to_string(),
        apps: apps
            .into_iter()
            .map(|(name, source)| {
                let AppFileSource::Written { dir, paths } = source else {
                    panic!("an install's file list names its files");
                };
                GenesisRegistryBundleApp {
                    owner: "acme".to_string(),
                    name,
                    version_hash: version.to_string(),
                    files: paths
                        .iter()
                        .map(|path| GenesisRegistryBundleFile {
                            content_base64: base64::engine::general_purpose::STANDARD
                                .encode(std::fs::read(dir.join(path)).expect("bundle file")),
                            path: path.clone(),
                        })
                        .collect(),
                }
            })
            .collect(),
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recovery made progress while the other response was withheld");
}

#[tokio::test]
async fn late_fetch_cannot_replace_a_newer_install() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let remote = tempfile::tempdir().expect("registry files");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-late-upgrade");
    let v1 = "1".repeat(40);
    write_app(remote.path(), "notes", "notes v1");
    let old_bundle = bundle(remote.path(), "notes", &v1);
    let mut old = record(remote.path(), &tenant, "notes", &v1);
    let (registry, arrivals) = Registry::serve(vec![old_bundle]).await;
    old.registry_url = registry.url.clone();
    ps.record_installed_app_metadata(&old).await.unwrap();
    let recovery = start_genesis_registry_cache_recovery(&platform).await;
    tokio::time::timeout(Duration::from_secs(10), arrivals[0].requested.notified())
        .await
        .expect("boot starts fetching the old version");

    install_pushed_app(
        &platform,
        &tenant,
        "notes",
        BTreeMap::from([
            (
                "app.toml".to_string(),
                PushedFile::Text("name = \"notes\"\n".to_string()),
            ),
            (
                "APP.md".to_string(),
                PushedFile::Text("# notes\n\nnotes v2\n".to_string()),
            ),
        ]),
    )
    .await
    .1
    .expect("a real upgrade completes while boot's old fetch waits");
    let upgraded = ps.get_installed_app(&tenant, "notes").await.unwrap();

    arrivals[0].release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), recovery.fetching.finished())
        .await
        .expect("obsolete recovery finishes");
    assert!(
        crate::os_apps::tenant_app_guide(&tenant, "notes")
            .unwrap()
            .contains("notes v2"),
        "late recovery must not replace the upgrade's catalog entry"
    );
    assert_eq!(
        ps.get_installed_app(&tenant, "notes").await.unwrap(),
        upgraded
    );
    crate::os_apps::restore_os_app(&platform, &tenant, "notes")
        .await
        .expect("the selected bundle still matches the upgraded record");
}

/// A policy forces runtime recovery to request replay; a guide-only app
/// would already count as ready and never exercise dependency installation.
fn write_parent(root: &Path) {
    write_app_needing(root, "notes", "notes v1", &["tags"]);
    std::fs::create_dir_all(root.join("notes/policies")).unwrap();
    std::fs::write(
        root.join("notes/policies/notes.cedar"),
        "permit(principal, action == Action::\"ReadNotes\", resource);",
    )
    .unwrap();
    written::keep_app(
        root,
        "notes",
        &["app.toml", "APP.md", "policies/notes.cedar"],
    );
}

#[tokio::test]
async fn late_dependency_retries_the_parent_runtime_recovery() {
    for (parent_cached, dependency_first) in [(false, false), (false, true), (true, false)] {
        let data_dir = tempfile::tempdir().expect("data dir");
        let remote = tempfile::tempdir().expect("registry files");
        let ps = Arc::new(store());
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-late-dependency-{parent_cached}-{dependency_first}");
        let (v1, v2) = ("1".repeat(40), "2".repeat(40));
        write_parent(remote.path());
        write_app(remote.path(), "tags", "tags v1");
        let notes_bundle = bundle(remote.path(), "notes", &v1);
        let mut notes = record(remote.path(), &tenant, "notes", &v1);
        write_app(remote.path(), "tags", "tags v2");
        let mut tags = record(remote.path(), &tenant, "tags", &v2);
        let mut tags_bundle = bundle(remote.path(), "tags", &v2);
        tags_bundle.apps.retain(|app| app.name == "tags");
        let (registry, arrivals) = Registry::serve(vec![notes_bundle, tags_bundle]).await;
        for record in [&mut notes, &mut tags] {
            record.registry_url = registry.url.clone();
            ps.record_installed_app_metadata(record).await.unwrap();
        }
        if parent_cached {
            let cached = cache(&platform, &tenant, "notes", &v1);
            write_parent(&cached);
            write_app(&cached, "tags", "tags v1");
        }
        let recovery = start_genesis_registry_cache_recovery(&platform).await;
        assert_eq!(recovery.restored, usize::from(parent_cached));
        for arrival in arrivals.iter().skip(usize::from(parent_cached)) {
            tokio::time::timeout(Duration::from_secs(10), arrival.requested.notified())
                .await
                .expect("the missing bundles are fetched independently");
        }
        let first = usize::from(dependency_first);
        arrivals[first].release.notify_one();
        let first_app = if dependency_first { "tags" } else { "notes" };
        wait_for(|| crate::os_apps::tenant_os_app(&tenant, first_app).is_some()).await;
        if !dependency_first {
            assert!(crate::os_apps::tenant_os_app(&tenant, "tags").is_none());
            assert!(
                platform
                    .server
                    .authz
                    .get_tenant_policy_text(&tenant)
                    .is_none()
            );
            crate::os_apps::restore_os_app(&platform, &tenant, "notes")
                .await
                .expect_err("the parent's first replay cannot find its upgraded dependency");
        }
        arrivals[1 - first].release.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), recovery.fetching.finished())
                .await
                .expect("recovery finishes after the required bundles arrive"),
            CacheFetchSummary {
                fetched: 2 - usize::from(parent_cached),
                failed: 0
            }
        );
        assert!(
            platform
                .server
                .authz
                .get_tenant_policy_text(&tenant)
                .unwrap_or_default()
                .contains("ReadNotes"),
            "the parent's runtime policy must recover in either arrival order"
        );
        assert_eq!(
            crate::os_apps::tenant_os_app_bundle_digest(&tenant, "tags")
                .unwrap()
                .bundle_digest,
            tags.bundle_digest,
            "the parent's old dependency never replaces the recorded upgrade"
        );
    }
}

/// Local apps also replay during boot, outside Genesis cache restoration.
/// Their registry dependencies may still be fetching when that replay runs.
#[tokio::test]
async fn late_dependency_retries_a_persisted_local_parent() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let local = tempfile::tempdir().expect("local apps");
    let remote = tempfile::tempdir().expect("registry files");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-late-local-dependency");
    let version = "2".repeat(40);

    write_parent(local.path());
    crate::os_apps::add_tenant_written_apps(
        &tenant,
        local.path().to_path_buf(),
        written::read(local.path()).expect("local file list"),
    );
    let notes = InstalledAppRecord {
        tenant: tenant.clone(),
        app_name: "notes".to_string(),
        source_kind: "local".to_string(),
        bundle_digest: written_digest(local.path(), "notes"),
        status: "installed".to_string(),
        ..Default::default()
    };
    ps.record_installed_app_metadata(&notes).await.unwrap();
    write_app(remote.path(), "tags", "tags v2");
    let (registry, arrivals) = Registry::serve(vec![bundle(remote.path(), "tags", &version)]).await;
    let tags = InstalledAppRecord {
        registry_url: registry.url.clone(),
        ..record(remote.path(), &tenant, "tags", &version)
    };
    ps.record_installed_app_metadata(&tags).await.unwrap();

    let recovery = tokio::time::timeout(
        Duration::from_secs(5),
        start_genesis_registry_cache_recovery(&platform),
    )
    .await
    .expect("boot returns while the local app's dependency is still fetching");
    assert_eq!(
        recovery.restored, 0,
        "local apps are not registry cache entries"
    );
    tokio::time::timeout(Duration::from_secs(10), arrivals[0].requested.notified())
        .await
        .expect("the dependency fetch starts");
    crate::os_apps::restore_os_app(&platform, &tenant, "notes")
        .await
        .expect_err("the local parent's boot replay cannot yet find its dependency");
    assert!(
        platform
            .server
            .authz
            .get_tenant_policy_text(&tenant)
            .is_none()
    );

    arrivals[0].release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), recovery.fetching.finished())
            .await
            .expect("dependency recovery finishes"),
        CacheFetchSummary {
            fetched: 1,
            failed: 0
        }
    );
    assert!(
        platform
            .server
            .authz
            .get_tenant_policy_text(&tenant)
            .unwrap_or_default()
            .contains("ReadNotes"),
        "the delayed registry dependency must revive the local parent's runtime policy"
    );
    assert_eq!(
        ps.get_installed_app(&tenant, "notes")
            .await
            .unwrap()
            .unwrap()
            .source_kind,
        "local",
        "retry preserves the parent's local provenance"
    );
    assert_eq!(
        ps.get_installed_app(&tenant, "tags").await.unwrap(),
        Some(tags)
    );
}
