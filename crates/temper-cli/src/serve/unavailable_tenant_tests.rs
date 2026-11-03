//! Startup leaves a tenant whose stored rows did not load exactly as stored.

use temper_platform::os_apps::add_os_apps_dir;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::storage::StorageStack;
use temper_store_turso::TursoEventStore;

use super::bootstrap::{EarlierOperatorKeys, bootstrap_tenants, build_registry};
use super::installed_apps::bootstrap_installed_apps;
use super::later::Later;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const UNLOADABLE_CONSTRAINTS: &str = "this is [[[ not toml";

/// Write an installable app whose own cross-invariants parse.
fn write_app(apps_root: &std::path::Path, app: &str) {
    let app_dir = apps_root.join(app);
    std::fs::create_dir_all(app_dir.join("specs")).expect("create specs dir"); // determinism-ok: test fixture
    let files = [
        (
            "app.toml",
            format!("name = \"{app}\"\ndescription = \"Startup test app\"\nversion = \"0.1.0\"\n"),
        ),
        ("APP.md", format!("# {app}\n\nStartup test app.\n")),
        ("specs/order.ioa.toml", ORDER_IOA.to_string()),
        ("specs/model.csdl.xml", ORDER_CSDL.to_string()),
        (
            "specs/cross-invariants.toml",
            "default_delete_policy = \"restrict\"\n".to_string(),
        ),
    ];
    for (path, text) in files {
        std::fs::write(app_dir.join(path), text).expect("write app file"); // determinism-ok: test fixture
    }
}

/// Every stored spec and constraints row for `tenant`, as stored.
async fn stored_rows(store: &TursoEventStore, tenant: &str) -> Vec<String> {
    let specs = store.load_specs().await.expect("load specs");
    let constraints = store
        .load_tenant_constraints()
        .await
        .expect("load constraints");
    let mut rows: Vec<String> = specs
        .iter()
        .filter(|row| row.tenant == tenant)
        .map(|row| format!("{row:?}"))
        .collect();
    rows.extend(
        constraints
            .iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| format!("{row:?}")),
    );
    rows
}

#[tokio::test]
async fn startup_keeps_an_unavailable_tenants_stored_rows_unchanged() {
    let id = temper_runtime::scheduler::sim_uuid();
    let tenant = "unloadable-tenant";
    let app = format!("startup-keeps-rows-{}", id.simple());
    let apps_root = std::env::temp_dir().join(format!("temper-startup-keeps-rows-{id}"));
    write_app(&apps_root, &app);
    add_os_apps_dir(apps_root.clone());

    let db_path = std::env::temp_dir().join(format!("temper-startup-keeps-rows-{id}.db"));
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("open store");
    let hash = temper_store_turso::spec_content_hash(ORDER_IOA);
    store
        .upsert_specs_and_commit(
            tenant,
            &[("Order", ORDER_IOA, ORDER_CSDL, &hash)],
            None,
            &app,
        )
        .await
        .expect("commit specs");
    store
        .upsert_tenant_constraints(tenant, UNLOADABLE_CONSTRAINTS)
        .await
        .expect("store constraints");
    store
        .record_installed_app(tenant, &app)
        .await
        .expect("record installed app");
    let before = stored_rows(&store, tenant).await;
    assert!(
        before.iter().any(|row| row.contains("not toml")),
        "{before:?}"
    );

    let stack = StorageStack::from_turso(store.clone());
    // The tenant is one of the server's own, so this boot restores it.
    let mut later = Later::new(Later::own_tenants(
        &[(tenant.to_string(), String::new())],
        None,
    ));
    let (registry, _) = build_registry(None, Some(&stack), &[], &mut later)
        .await
        .expect("startup restores the other tenants");
    assert!(
        registry
            .tenant_unavailable_reason(&TenantId::new(tenant))
            .is_some()
    );
    let mut state = PlatformState::with_registry(registry, None);
    state.server.set_storage_stack(stack);
    bootstrap_tenants(&state, &[], EarlierOperatorKeys::Keep, &later).await;
    bootstrap_installed_apps(&state, &[], &mut later)
        .await
        .expect("startup continues past the unavailable tenant");

    assert_eq!(
        stored_rows(&store, tenant).await,
        before,
        "startup must not rewrite an unavailable tenant's stored rows"
    );

    let _ = std::fs::remove_file(&db_path); // determinism-ok: test cleanup
    let _ = std::fs::remove_dir_all(&apps_root); // determinism-ok: test cleanup
}
