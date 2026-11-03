//! An overdue install timer cannot dispatch while module storage is awaiting
//! after its spec went live (`Swap::SpecsFirst`), not even when another
//! tenant's install finishes in the meantime and starts a walk of its own.
use super::*;
use std::sync::Arc;
use temper_runtime::persistence::{EventStore, PersistenceError};
use temper_server::storage::TursoStoreProvider;
use temper_store_turso::{TenantUserRow, TursoEventStore};

tokio::task_local! { static INSTALLER: bool; }

struct ObserveModuleInstall {
    inner: Arc<dyn TursoStoreProvider>,
    modules: temper_server::ServerState,
    tenant: TenantId,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl TursoStoreProvider for ObserveModuleInstall {
    fn supports_tenant_admin(&self) -> bool {
        self.inner.supports_tenant_admin()
    }
    fn platform_store(&self) -> Option<TursoEventStore> {
        self.inner.platform_store()
    }
    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        let spec_changed = self
            .modules
            .registry
            .read()
            .expect("registry lock")
            .get_spec(&self.tenant, "Job")
            .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty());
        if tenant == self.tenant.as_str()
            && spec_changed
            && INSTALLER.try_with(|installer| *installer).unwrap_or(false)
        {
            self.entered.notify_one();
            let _ = self.release.acquire().await;
        }
        self.inner.store_for_tenant(tenant).await
    }
    async fn all_stores(&self) -> Vec<TursoEventStore> {
        self.inner.all_stores().await
    }
    async fn connected_tenants(&self) -> Vec<String> {
        self.inner.connected_tenants().await
    }
    async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.tenants_for_user(user_id).await
    }
    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        self.inner.register_tenant(tenant_id).await
    }
    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        self.inner.list_tenants().await
    }
    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.remove_tenant(tenant_id).await
    }
    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.add_tenant_user(tenant_id, user_id, role).await
    }
    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.list_tenant_users(tenant_id).await
    }
    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.remove_tenant_user(tenant_id, user_id).await
    }
    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.ensure_tenant(tenant_id).await
    }
}

#[tokio::test]
async fn overdue_install_timer_waits_for_module_registration() {
    overdue_install_timer_waits("timer-install-module-registration", None).await;
}

/// Tenant B installs the same app, and its walk runs, while tenant A's
/// install waits for module storage: A's changed spec must stay out of B's
/// walk, so A's overdue timer neither fires early nor is spent on a refused
/// fire, and A's own install arms it once its module is in.
#[tokio::test]
async fn overdue_install_timer_waits_while_another_tenant_installs() {
    overdue_install_timer_waits("timer-install-cross-tenant", Some("timer-install-b")).await;
}

async fn overdue_install_timer_waits(app: &str, other_tenant: Option<&str>) {
    let plan = OsAppInstallPlan {
        specs: true,
        policies: true,
        wasm: true,
        content: false,
        seed: false,
    };
    let root = std::env::temp_dir().join(format!("timer-install-{}", uuid::Uuid::new_v4()));
    let dir = root.join(app);
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::create_dir_all(dir.join("wasm/echo")).expect("create the echo module dir");
    fs::create_dir_all(dir.join("policies")).expect("create policies dir");
    fs::write(
        dir.join("policies/allow.cedar"),
        "permit(principal, action, resource);",
    )
    .expect("write policy");
    fs::write(
        dir.join("app.toml"),
        format!(
            r#"name = "{app}"
version = "0.1.0"
[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#
        ),
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Timer install").expect("write APP.md");
    let wasm = fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../temper-wasm/tests/fixtures/echo_integration.wasm"),
    )
    .expect("read echo fixture");
    fs::write(dir.join("wasm/echo/echo.wasm"), &wasm).expect("write echo module");
    // `Poke` counts itself, so it changes the record and its event is
    // written: a machine's move that changes nothing writes none.
    let spec = r#"
[automaton]
name = "Job"
states = ["Waiting", "Done"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Done"]
[[state]]
name = "pokes"
type = "counter"
initial = 0
[[action]]
name = "Poke"
from = ["Waiting"]
effect = ["pokes += 1"]
[[action]]
name = "Expire"
from = ["Waiting"]
to = "Done"
"#;
    let csdl = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Timers" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Timers.Job"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
    fs::write(dir.join("specs/model.csdl.xml"), csdl).expect("write csdl");
    fs::write(dir.join("specs/job.ioa.toml"), spec).expect("write spec");
    let store = TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
        .await
        .expect("open turso store");
    let mut state = PlatformState::new(None);
    state.server.data_dir = root.join("data");
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
    let tenant = TenantId::new("timer-install");
    let other = other_tenant.map(TenantId::new);
    for each in std::iter::once(&tenant).chain(other.as_ref()) {
        register_untimed(&state, each, csdl, spec);
    }
    state
        .server
        .start_runtime()
        .await
        .expect("first start")
        .await
        .expect("boot walk");
    state
        .server
        .get_or_create_tenant_entity(&tenant, "Job", "r", serde_json::json!({}))
        .await
        .expect("create record");
    let response = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Job",
            "r",
            "Poke",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch Poke");
    assert!(
        response.success,
        "initial dispatch failed: {:?}",
        response.error
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut stack = state
        .server
        .storage_stack
        .as_ref()
        .expect("storage stack")
        .as_ref()
        .clone();
    stack.turso = Some(Arc::new(ObserveModuleInstall {
        inner: stack.turso.take().expect("turso provider"),
        modules: state.server.clone(),
        tenant: tenant.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    state.server.set_storage_stack(stack);
    // The record predates the newly installed one-second budget.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fs::write(
        dir.join("specs/job.ioa.toml"),
        format!(
            r#"{spec}
[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Expire"
"#
        ),
    )
    .expect("write timed spec");
    add_os_apps_dir(root.clone());
    // Specs first, as a version put back goes in: its timers are live while
    // its modules are still being stored, which the hold must cover.
    let install = INSTALLER.scope(
        true,
        install_os_app_swapping(
            &state,
            tenant.as_str(),
            app,
            plan,
            InstallOrigin::Request,
            Swap::SpecsFirst,
        ),
    );
    tokio::pin!(install);
    tokio::select! {
        _ = entered.notified() => {},
        result = &mut install => panic!("installation missed the storage pause: {result:?}"),
    }
    if let Some(other) = &other {
        // B's install ends with a walk of its own while A's still waits.
        install_os_app_with_plan(&state, other.as_str(), app, plan, InstallOrigin::Request)
            .await
            .expect("tenant B installs while tenant A waits for module storage");
    }
    assert!(
        state
            .server
            .wasm_module_registry
            .read()
            .expect("module registry lock")
            .get_hash(&tenant, "echo")
            .is_none()
    );
    // Hold module storage long enough for an erroneously early timer to fire.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        store
            .read_events("timer-install:Job:r", 0)
            .await
            .expect("read events")
            .len(),
        2,
        "overdue timeout dispatched while module installation was blocked"
    );
    release.close();
    install.await.expect("install the app");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .read_events("timer-install:Job:r", 0)
                .await
                .expect("read events")
                .len()
                == 3
            {
                let record = state
                    .server
                    .get_tenant_entity_state(&tenant, "Job", "r")
                    .await
                    .expect("read record");
                assert_eq!(record.state.status, "Done");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the overdue record must leave its timed state after installation");
    fs::remove_dir_all(root).expect("remove temp dir");
}

/// Register `tenant` with the app's untimed spec, verified, under `allow`.
fn register_untimed(state: &PlatformState, tenant: &TenantId, csdl: &str, spec: &str) {
    let mut registry = state.registry.write().expect("registry lock");
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(csdl).expect("parse csdl"),
        csdl.to_string(),
        &[("Job", spec)],
    );
    registry.set_verification_status(
        tenant,
        "Job",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-24T00:00:00Z".to_string(),
        }),
    );
    drop(registry);
    state
        .server
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("load policies");
}
