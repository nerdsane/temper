//! Boot and pushed app versions: an `--app` with no files waits for its first
//! install.

use std::path::Path;

use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::storage::StorageStack;
use temper_store_turso::TursoEventStore;

use super::installed_apps::{bootstrap_installed_apps, missing_app_line};
use super::later::Later;

const TENANT: &str = "default";

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

    bootstrap_installed_apps(
        &state,
        std::slice::from_ref(&app),
        &mut Later::new(Later::own_tenants(&[], None)),
    )
    .await
    .expect("an app with no files does not stop the boot");

    assert!(registered_order(&state).is_none());
    assert!(
        stored_install(&state, &app).await.is_none(),
        "nothing is stored for an app with no files"
    );
    assert!(
        !data_dir.path().join("genesis-app-cache").exists(),
        "no version is written for an app with no files"
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
