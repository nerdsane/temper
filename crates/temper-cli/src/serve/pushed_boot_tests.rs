//! Boot and pushed app versions: an `--app` with no files waits for its first
//! install, and an `--app` served from the boot catalog with no pushed version
//! is adopted as its first one, which later boots replay as recorded.

use std::path::Path;

use temper_platform::os_apps::add_os_apps_dir;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::storage::StorageStack;
use temper_store_turso::TursoEventStore;

use super::bootstrap::{bootstrap_installed_apps, missing_app_line};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const TENANT: &str = "default";

fn write_app(apps_root: &Path, app: &str, spec: &str) {
    let app_dir = apps_root.join(app);
    std::fs::create_dir_all(app_dir.join("specs")).expect("create specs dir"); // determinism-ok: test fixture
    let files = [
        (
            "app.toml",
            format!("name = \"{app}\"\ndescription = \"Boot test app\"\nversion = \"0.1.0\"\n"),
        ),
        ("APP.md", format!("# {app}\n\nBoot test app.\n")),
        ("specs/order.ioa.toml", spec.to_string()),
        ("specs/model.csdl.xml", ORDER_CSDL.to_string()),
    ];
    for (path, text) in files {
        std::fs::write(app_dir.join(path), text).expect("write app file"); // determinism-ok: test fixture
    }
}

/// A server whose data directory is `data_dir` and whose database is the file
/// `db`, so two servers on the same pair are one server restarted.
async fn server(data_dir: &Path, db: &Path) -> PlatformState {
    let mut state = PlatformState::new(None);
    state.server.data_dir = data_dir.to_path_buf();
    let store = TursoEventStore::new(&format!("file:{}", db.display()), None)
        .await
        .expect("open store");
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store));
    state
}

fn registered_order(state: &PlatformState) -> Option<String> {
    state
        .registry
        .read()
        .expect("registry")
        .get_spec(&TenantId::new(TENANT), "Order")
        .map(|spec| spec.ioa_source.clone())
}

/// `(source_kind, app_ref)` of the install `state` stored for `app`.
async fn stored_install(state: &PlatformState, app: &str) -> Option<(String, String)> {
    let store = state.server.storage_stack.as_ref()?.platform.clone()?;
    let record = store.get_installed_app(TENANT, app).await.ok()??;
    Some((record.source_kind, record.app_ref))
}

#[tokio::test]
async fn an_app_with_no_files_anywhere_waits_and_boot_goes_on() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let app = format!("waits-{}", temper_runtime::scheduler::sim_uuid().simple());
    let state = server(data_dir.path(), &data_dir.path().join("temper.db")).await;

    bootstrap_installed_apps(&state, std::slice::from_ref(&app))
        .await
        .expect("an app with no files does not stop the boot");

    assert!(registered_order(&state).is_none());
    assert!(
        stored_install(&state, &app).await.is_none(),
        "nothing is adopted for an app with no files"
    );
    assert!(
        !data_dir.path().join("genesis-app-cache").exists(),
        "no version is written for an app with no files"
    );
}

#[tokio::test]
async fn a_catalog_app_is_adopted_and_replayed_on_later_boots_whatever_the_catalog_holds() {
    let id = temper_runtime::scheduler::sim_uuid();
    let app = format!("adopted-{}", id.simple());
    let apps_root = tempfile::tempdir().expect("apps root");
    let data_dir = tempfile::tempdir().expect("data dir");
    let db = data_dir.path().join("temper.db");
    let served = format!("{ORDER_IOA}\n# the version served before pushes\n");
    write_app(apps_root.path(), &app, &served);
    let commit = "0123456789abcdef0123456789abcdef01234567\n";
    std::fs::write(apps_root.path().join(&app).join("COMMIT"), commit).expect("write COMMIT"); // determinism-ok: test fixture
    add_os_apps_dir(apps_root.path().to_path_buf());

    let first = server(data_dir.path(), &db).await;
    bootstrap_installed_apps(&first, std::slice::from_ref(&app))
        .await
        .expect("first boot");
    assert_eq!(registered_order(&first).as_deref(), Some(served.as_str()));
    let (kind, adopted) = stored_install(&first, &app)
        .await
        .expect("the served version is adopted");
    assert_eq!(kind, "pushed");
    assert!(
        adopted.starts_with(&format!("pushed/{app}@")),
        "adopted as a pushed version: {adopted}"
    );

    // The adopted version is a whole copy of the catalog's app, its COMMIT too.
    let roots: Vec<_> = std::fs::read_dir(data_dir.path().join("genesis-app-cache")) // determinism-ok: test reads the server's files
        .expect("cache dir")
        .flatten()
        .map(|entry| entry.path().join(&app))
        .filter(|dir| dir.is_dir())
        .collect();
    assert_eq!(roots.len(), 1, "one adopted version: {roots:?}");
    assert_eq!(
        std::fs::read_to_string(roots[0].join("specs/order.ioa.toml")).expect("adopted spec"), // determinism-ok: test reads the server's files
        served
    );
    assert_eq!(
        std::fs::read_to_string(roots[0].join("COMMIT")).expect("adopted COMMIT"), // determinism-ok: test reads the server's files
        commit,
        "the adopted version names the catalog's commit"
    );

    // The catalog moves on. A restart replays the stored pushed install rather
    // than installing the catalog's files over it: it still serves the adopted
    // version, its record still names it, and nothing is adopted again. (In
    // this process the adopted root stays registered from the first boot; a
    // new process gets it back from boot restore, which the platform's pushed
    // tests cover.)
    write_app(
        apps_root.path(),
        &app,
        &format!("{ORDER_IOA}\n# a later catalog\n"),
    );
    let second = server(data_dir.path(), &db).await;
    bootstrap_installed_apps(&second, std::slice::from_ref(&app))
        .await
        .expect("second boot");
    assert_eq!(registered_order(&second).as_deref(), Some(served.as_str()));
    assert_eq!(
        stored_install(&second, &app).await,
        Some(("pushed".to_string(), adopted))
    );
}

/// With no files for an `--app`, boot says it waits only when it serves
/// nothing for the app; an app whose install it stored serves those specs.
#[test]
fn a_missing_app_waits_only_when_nothing_was_stored_for_it() {
    assert_eq!(
        missing_app_line("shop", TENANT, false),
        "  App 'shop' waits for its first install for 'default'"
    );
    assert_eq!(
        missing_app_line("shop", TENANT, true),
        "  App 'shop' serves its stored specs for 'default'; no files to install"
    );
}
