//! A bundle whose specs cannot be registered is refused for its own tenant:
//! nothing is stored for it, and every other tenant keeps working.

use super::*;
use std::path::PathBuf;

use temper_store_turso::TursoEventStore;

const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
/// Well-formed TOML the cross-invariant parser does not accept: the server
/// does not carry out `setnull`.
const UNSUPPORTED_CROSS_INVARIANTS: &str = "default_delete_policy = \"setnull\"\n";

struct Fixture {
    state: PlatformState,
    store: TursoEventStore,
    apps_root: PathBuf,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let id = uuid::Uuid::new_v4();
        let db_path = std::env::temp_dir().join(format!("temper-bundle-refusal-{label}-{id}.db"));
        let db_url = format!("file:{}", db_path.display());
        let apps_root = std::env::temp_dir().join(format!("temper-bundle-refusal-{label}-{id}"));
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("open the bundle refusal database");
        let mut state = PlatformState::new(None);
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
        Self {
            state,
            store,
            apps_root,
        }
    }

    /// Write an app with the Order spec, `csdl` and optional cross-invariants.
    fn write_app(&self, app: &str, csdl: &str, cross_invariants: Option<&str>) {
        let app_dir = self.apps_root.join(app);
        fs::create_dir_all(app_dir.join("specs")).expect("create specs dir");
        fs::write(
            app_dir.join("app.toml"),
            format!(
                "name = \"{app}\"\ndescription = \"Bundle refusal test app\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("write app.toml");
        fs::write(app_dir.join("APP.md"), format!("# {app}\n\nTest app.\n")).expect("write APP.md");
        fs::write(app_dir.join("specs/order.ioa.toml"), ORDER_IOA).expect("write spec");
        fs::write(app_dir.join("specs/model.csdl.xml"), csdl).expect("write CSDL");
        if let Some(text) = cross_invariants {
            fs::write(app_dir.join("specs/cross-invariants.toml"), text)
                .expect("write cross-invariants");
        }
    }

    async fn install(&self, tenant: &str, app: &str) -> Result<InstallResult, String> {
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, tenant, app).await
    }

    async fn stored_spec_tenants(&self) -> Vec<String> {
        let rows = self.store.load_specs().await.expect("load specs");
        rows.into_iter().map(|row| row.tenant).collect()
    }

    async fn stored_constraint_tenants(&self) -> Vec<String> {
        let rows = self
            .store
            .load_tenant_constraints()
            .await
            .expect("load constraints");
        rows.into_iter().map(|row| row.tenant).collect()
    }

    fn other_tenant_is_served(&self, tenant: &str) -> bool {
        self.state
            .server
            .check_verification_gate(&TenantId::new(tenant), "Order")
            .is_ok()
    }
}

#[tokio::test]
async fn a_bundle_with_unparseable_cross_invariants_is_refused_and_other_tenants_keep_working() {
    let fx = Fixture::new("cross-invariants").await;
    let good = format!("refusal-good-{}", uuid::Uuid::new_v4().simple());
    let bad = format!("refusal-bad-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&good, ORDER_CSDL, None);
    fx.write_app(&bad, ORDER_CSDL, Some(UNSUPPORTED_CROSS_INVARIANTS));

    fx.install("tenant-b", &good)
        .await
        .expect("the other tenant's app installs");
    assert!(fx.other_tenant_is_served("tenant-b"));

    let error = fx
        .install("tenant-a", &bad)
        .await
        .expect_err("a bundle whose cross-invariants do not parse is refused");
    assert!(error.contains("cross-invariants"), "{error}");

    assert!(
        !fx.state.registry.is_poisoned(),
        "a refused install leaves the shared registry usable"
    );
    assert!(fx.other_tenant_is_served("tenant-b"));
    assert!(
        !fx.stored_spec_tenants()
            .await
            .contains(&"tenant-a".to_string()),
        "a refused install stores no spec rows"
    );
    assert!(
        !fx.stored_constraint_tenants()
            .await
            .contains(&"tenant-a".to_string()),
        "a refused install stores no constraints row"
    );

    fx.install("tenant-b", &good)
        .await
        .expect("installs keep working after a refused one");
}

#[tokio::test]
async fn a_new_tenants_bundle_with_unparseable_csdl_stores_nothing() {
    let fx = Fixture::new("csdl").await;
    let bad = format!("refusal-csdl-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&bad, "<not-csdl", None);

    let error = fx
        .install("tenant-new", &bad)
        .await
        .expect_err("a bundle whose CSDL does not parse is refused");
    assert!(error.contains("CSDL"), "{error}");
    assert!(
        !fx.stored_spec_tenants()
            .await
            .contains(&"tenant-new".to_string()),
        "a refused install stores no spec rows"
    );
}

#[tokio::test]
async fn an_install_into_an_unavailable_tenant_is_refused_and_stores_nothing() {
    let fx = Fixture::new("unavailable").await;
    let good = format!("refusal-unavailable-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&good, ORDER_CSDL, None);
    fx.state
        .registry
        .write()
        .expect("registry writable")
        .mark_tenant_unavailable("tenant-held", "stored constraints".to_string());

    let error = fx
        .install("tenant-held", &good)
        .await
        .expect_err("a tenant kept for repair gets no install");
    assert!(error.contains("unavailable"), "{error}");
    assert!(
        !fx.stored_spec_tenants()
            .await
            .contains(&"tenant-held".to_string()),
        "a refused install stores no spec rows"
    );
}

#[test]
fn registering_unparseable_cross_invariants_returns_an_error_and_keeps_the_registry_usable() {
    let state = PlatformState::new(None);
    let verified_cache = BTreeMap::new();
    let result = bootstrap::bootstrap_tenant_specs(
        &state,
        "tenant-a",
        ORDER_CSDL,
        &[("Order", ORDER_IOA)],
        bootstrap::BootstrapTenantSpecsOptions {
            merge: true,
            csdl_mode: bootstrap::CsdlMode::MergedIntoExisting,
            label: "OsApp(test)",
            verified_cache: &verified_cache,
            cross_invariants_source: Some(UNSUPPORTED_CROSS_INVARIANTS),
            verification_mode: bootstrap::BootstrapSpecVerificationMode::TrustBundle,
        },
    );

    let error = result.expect_err("unparseable cross-invariants are an error");
    assert!(error.contains("cross-invariants"), "{error}");
    assert!(!state.registry.is_poisoned());
    assert!(
        state
            .registry
            .read()
            .expect("registry readable")
            .get_tenant(&TenantId::new("tenant-a"))
            .is_none(),
        "a refused registration changes nothing"
    );
}

#[path = "bundle_refusal/trigger_args_test.rs"]
mod trigger_args;

#[path = "bundle_refusal/principal_collision_test.rs"]
mod principal_collision;
