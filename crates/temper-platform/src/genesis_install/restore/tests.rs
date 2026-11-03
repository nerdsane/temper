//! Restoring an install reuses a cache directory only while it holds the recorded versions:
//! each app in it is taken while it matches its own record.

use std::sync::Arc;

use super::super::written;
use super::*;
use crate::os_apps::written_apps_bundle_digests;
use temper_server::platform_store::{SimPlatformFaultConfig, SimPlatformStore};

const TENANT: &str = "restore-test-tenant";

fn write_app(root: &Path, app_name: &str, marker: &str) {
    write_app_needing(root, app_name, marker, &[]);
}

/// `app_name` in `root`, declaring `dependencies` in its app.toml.
fn write_app_needing(root: &Path, app_name: &str, marker: &str, dependencies: &[&str]) {
    let app_dir = root.join(app_name);
    std::fs::create_dir_all(&app_dir).expect("app dir");
    let dependencies = dependencies
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        app_dir.join("app.toml"),
        format!("name = \"{app_name}\"\ndependencies = [{dependencies}]\n"),
    )
    .expect("app.toml");
    std::fs::write(
        app_dir.join("APP.md"),
        format!("# {app_name}\n\n{marker}\n"),
    )
    .expect("APP.md");
    written::keep_app(root, app_name, &["app.toml", "APP.md"]);
}

/// `app_name`'s bundle digest as `root`'s list names its files.
pub(super) fn written_digest(root: &Path, app_name: &str) -> String {
    let apps = written::read(root).expect("the list");
    written_apps_bundle_digests(root, apps)
        .into_iter()
        .find(|digest| digest.app_name == app_name)
        .expect("digest")
        .bundle_digest
}

/// `app_name`'s record as installed from `root` now.
fn recorded(root: &Path, app_name: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        tenant: TENANT.to_string(),
        app_name: app_name.to_string(),
        source_kind: "genesis".to_string(),
        bundle_digest: written_digest(root, app_name),
        ..Default::default()
    }
}

fn store() -> SimPlatformStore {
    SimPlatformStore::new(0, SimPlatformFaultConfig::none())
}

fn names(apps: &[&str]) -> BTreeSet<String> {
    apps.iter().map(ToString::to_string).collect()
}

/// The apps a cache directory serves, by name.
async fn matching(
    ps: &SimPlatformStore,
    root: &Path,
    record: &InstalledAppRecord,
) -> Option<BTreeSet<String>> {
    let only = std::slice::from_ref(record);
    let apps = cached_apps_matching_records(ps, root, record, only).await?;
    Some(apps.into_iter().map(|(app, _)| app).collect())
}

#[tokio::test]
async fn a_cache_directory_is_reused_only_while_it_holds_the_recorded_version() {
    let root = tempfile::tempdir().expect("cache root");
    write_app(root.path(), "notes", "first version");
    let record = recorded(root.path(), "notes");
    let ps = store();
    assert_eq!(
        matching(&ps, root.path(), &record).await,
        Some(names(&["notes"]))
    );

    write_app(root.path(), "notes", "another version");
    assert_eq!(matching(&ps, root.path(), &record).await, None);
    let gone = root.path().join("gone");
    assert_eq!(matching(&ps, &gone, &record).await, None);
}

#[tokio::test]
async fn a_cache_directory_serves_only_the_apps_that_match_their_records() {
    let root = tempfile::tempdir().expect("cache root");
    write_app(root.path(), "notes", "first version");
    write_app(root.path(), "tags", "first version");
    let notes = recorded(root.path(), "notes");
    let ps = store();
    ps.record_installed_app_metadata(&recorded(root.path(), "tags"))
        .await
        .expect("record tags");
    assert_eq!(
        matching(&ps, root.path(), &notes).await,
        Some(names(&["notes", "tags"]))
    );

    write_app(root.path(), "tags", "another version");
    assert_eq!(
        matching(&ps, root.path(), &notes).await,
        Some(names(&["notes"])),
        "a dependency at another version than its record is left out, not the directory"
    );

    write_app(root.path(), "tags", "first version");
    write_app(root.path(), "unrecorded", "no record names it");
    assert_eq!(
        matching(&ps, root.path(), &notes).await,
        Some(names(&["notes", "tags"])),
        "an app no record names is left out"
    );
}

#[tokio::test]
async fn rollback_uses_the_cache_an_app_install_made() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let app_ref = format!("acme/notes@{}", "1".repeat(40));
    let local = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: TENANT,
            source: InstallSource::Local("default"),
        },
        &app_ref,
    );
    write_app(&local, "notes", "prior local version");
    let prior = InstalledAppRecord {
        app_ref,
        registry_tenant: "default".to_string(),
        ..recorded(&local, "notes")
    };

    ensure_prior_bundle_available(&platform, &store(), &prior, std::slice::from_ref(&prior))
        .await
        .expect("the prior bundle is available");

    let guide = crate::os_apps::tenant_app_guide(TENANT, "notes")
        .expect("the tenant resolves the prior notes");
    assert!(guide.contains("prior local version"), "{guide}");
}

/// A fresh server over `ps` and `data_dir`.
fn server_on(data_dir: &Path, ps: &Arc<SimPlatformStore>) -> PlatformState {
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.to_path_buf();
    platform
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(
            temper_store_sim::SimEventStore::no_faults(0),
            Some(ps.clone()),
        ));
    platform
}

/// One boot of a fresh server over `ps` and `data_dir`, restoring the stored
/// installs the way `temper serve` does: cache roots first, then a replay of
/// each stored install. Returns how many Genesis installs it restored.
async fn boot(data_dir: &Path, ps: &Arc<SimPlatformStore>) -> usize {
    let platform = server_on(data_dir, ps);
    let restored = restore_genesis_registry_cache_roots(&platform).await;
    for (tenant, app_name) in ps.list_all_installed_apps().await.expect("installed apps") {
        crate::os_apps::restore_os_app(&platform, &tenant, &app_name)
            .await
            .expect("the stored install replays");
    }
    restored
}

#[tokio::test]
async fn a_genesis_install_is_restored_as_recorded_after_every_restart() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-restarts");
    let version = "2".repeat(40);
    let app_ref = format!("acme/notes@{version}");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let cache = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: &tenant,
            source: InstallSource::Registry("default"),
        },
        &app_ref,
    );
    write_app(&cache, "notes", "registry version");
    let installed = InstalledAppRecord {
        tenant: tenant.clone(),
        app_ref: app_ref.clone(),
        version_hash: version.clone(),
        pinned_version_hash: version.clone(),
        current_version_hash: version.clone(),
        follow_policy: "pinned".to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        registry_tenant: "default".to_string(),
        status: "installed".to_string(),
        ..recorded(&cache, "notes")
    };
    let ps = Arc::new(store());
    ps.record_installed_app_metadata(&installed)
        .await
        .expect("record the Genesis install");

    for restart in 1..=3 {
        assert_eq!(
            boot(data_dir.path(), &ps).await,
            1,
            "restart {restart} restores the Genesis install"
        );
        let stored = ps
            .get_installed_app(&tenant, "notes")
            .await
            .expect("read the install record")
            .expect("the install record is kept");
        assert_eq!(stored.source_kind, "genesis", "restart {restart}");
        assert_eq!(stored.app_ref, app_ref, "restart {restart}");
        assert_eq!(stored.version_hash, version, "restart {restart}");
        assert_eq!(
            stored.registry_url, installed.registry_url,
            "restart {restart}"
        );
        assert_eq!(stored.registry_tenant, "default", "restart {restart}");
        assert_eq!(
            stored.bundle_digest, installed.bundle_digest,
            "restart {restart}"
        );
    }
}

/// A cache written before installs kept their file list restores from one
/// listing, with no fetch: its registry is a port nothing answers on, and the
/// restore still counts it. The listing becomes its list, so the next boot
/// reads the list.
#[tokio::test]
async fn a_cache_from_before_file_lists_restores_without_a_fetch_and_keeps_a_list() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-before-lists");
    let version = "3".repeat(40);
    let app_ref = format!("acme/notes@{version}");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let cache = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: &tenant,
            source: InstallSource::Registry("default"),
        },
        &app_ref,
    );
    write_app(&cache, "notes", "cached before lists");
    let installed = InstalledAppRecord {
        tenant: tenant.clone(),
        app_ref,
        version_hash: version.clone(),
        pinned_version_hash: version.clone(),
        current_version_hash: version,
        follow_policy: "pinned".to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        registry_tenant: "default".to_string(),
        status: "installed".to_string(),
        ..recorded(&cache, "notes")
    };
    let list = cache.join(".written.json");
    std::fs::remove_file(&list).expect("a cache from before lists has none");
    let ps = Arc::new(store());
    ps.record_installed_app_metadata(&installed)
        .await
        .expect("record the Genesis install");

    assert_eq!(
        boot(data_dir.path(), &ps).await,
        1,
        "restored with no fetch"
    );
    assert!(list.is_file(), "the listing became its list");
    assert_eq!(
        written_digest(&cache, "notes"),
        installed.bundle_digest,
        "the kept list names the recorded version's files"
    );
    assert_eq!(boot(data_dir.path(), &ps).await, 1, "and restores from it");
}

/// A cache from before file lists whose listing does not read back at the
/// recorded version is not used, and gets no list: the caller fetches it.
#[tokio::test]
async fn a_cache_from_before_file_lists_at_another_version_gets_no_list() {
    let root = tempfile::tempdir().expect("cache root");
    write_app(root.path(), "notes", "first version");
    let record = recorded(root.path(), "notes");
    write_app(root.path(), "notes", "another version");
    let list = root.path().join(".written.json");
    std::fs::remove_file(&list).expect("no list");
    assert_eq!(matching(&store(), root.path(), &record).await, None);
    assert!(!list.exists());
}

#[tokio::test]
async fn a_replay_installs_nothing_when_only_another_bundle_of_the_name_is_here() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-stand-in");
    let app_name = "restore-stand-in-notes";
    let version = "3".repeat(40);
    // The recorded Genesis version, whose files this server no longer has.
    let registry_files = tempfile::tempdir().expect("registry files");
    write_app(registry_files.path(), app_name, "registry version");
    let installed = InstalledAppRecord {
        tenant: tenant.clone(),
        app_ref: format!("acme/{app_name}@{version}"),
        version_hash: version.clone(),
        registry_url: "http://127.0.0.1:9".to_string(),
        registry_tenant: "default".to_string(),
        status: "installed".to_string(),
        ..recorded(registry_files.path(), app_name)
    };
    // Another bundle of the same name, in the catalog every tenant shares.
    let shared = tempfile::tempdir().expect("shared apps");
    write_app(shared.path(), app_name, "shared version");
    crate::os_apps::add_os_apps_dir(shared.path().to_path_buf());
    let ps = Arc::new(store());
    ps.record_installed_app_metadata(&installed)
        .await
        .expect("record the Genesis install");

    let platform = server_on(data_dir.path(), &ps);
    let error = crate::os_apps::restore_os_app(&platform, &tenant, app_name)
        .await
        .expect_err("the replay refuses the shared bundle");
    assert!(error.contains("unavailable"), "{error}");
    assert!(error.contains(&installed.bundle_digest), "{error}");

    let stored = ps
        .get_installed_app(&tenant, app_name)
        .await
        .expect("read the install record")
        .expect("the install record is kept");
    assert_eq!(stored.source_kind, "genesis");
    assert_eq!(stored.app_ref, installed.app_ref);
    assert_eq!(stored.bundle_digest, installed.bundle_digest);
}

/// An app installed with a dependency, whose dependency was then upgraded on
/// its own, comes back offline at the versions its records name: the app from
/// its install's cache, the dependency at its upgraded version from its own.
#[tokio::test]
async fn an_app_comes_back_beside_a_dependency_upgraded_on_its_own() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-upgraded-dependency");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let cache_for = |app_ref: &str| {
        genesis_cache_root(
            &platform.server,
            CacheOwner::Install {
                tenant: &tenant,
                source: InstallSource::Registry("default"),
            },
            app_ref,
        )
    };
    // The registry is not reachable: everything comes from the caches.
    let genesis_record = |cache: &Path, app_name: &str, version: &str| InstalledAppRecord {
        tenant: tenant.clone(),
        app_ref: format!("acme/{app_name}@{version}"),
        version_hash: version.to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        registry_tenant: "default".to_string(),
        status: "installed".to_string(),
        ..recorded(cache, app_name)
    };
    let (v1, v2) = ("1".repeat(40), "2".repeat(40));

    // notes installed at v1, bringing tags v1 with it.
    let notes_cache = cache_for(&format!("acme/notes@{v1}"));
    write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
    write_app(&notes_cache, "tags", "tags v1");
    // tags then upgraded to v2 on its own.
    let tags_cache = cache_for(&format!("acme/tags@{v2}"));
    write_app(&tags_cache, "tags", "tags v2");

    let notes = genesis_record(&notes_cache, "notes", &v1);
    let tags = genesis_record(&tags_cache, "tags", &v2);
    let ps = Arc::new(store());
    for record in [&notes, &tags] {
        ps.record_installed_app_metadata(record)
            .await
            .expect("record the Genesis install");
    }

    assert_eq!(boot(data_dir.path(), &ps).await, 2, "both installs restore");
    let guide = |app_name: &str| {
        crate::os_apps::tenant_app_guide(&tenant, app_name)
            .unwrap_or_else(|| panic!("the tenant resolves {app_name}"))
    };
    assert!(guide("notes").contains("notes v1"), "{}", guide("notes"));
    assert!(guide("tags").contains("tags v2"), "{}", guide("tags"));
    for record in [&notes, &tags] {
        let stored = ps
            .get_installed_app(&tenant, &record.app_name)
            .await
            .expect("read the install record")
            .expect("the install record is kept");
        assert_eq!(stored.app_ref, record.app_ref);
        assert_eq!(stored.bundle_digest, record.bundle_digest);
    }
}

#[path = "rollback_tests.rs"]
mod rollback_tests;

#[path = "legacy_cache_tests.rs"]
mod legacy_cache_tests;

#[path = "upgrade_owner_tests.rs"]
mod upgrade_owner_tests;

#[path = "stall_tests.rs"]
mod stall_tests;

#[path = "late_fetch_tests.rs"]
mod late_fetch_tests;
