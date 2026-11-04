//! A boot replays every recorded install. An install whose bundle is the one
//! it recorded, and whose specs, Cedar and modules the boot already restored
//! from the store, is registered: nothing is installed again, its
//! dependencies included, and nothing in the store changes. The tenant then
//! serves what it served before the restart. A bundle that changed since its
//! install is installed again.
//!
//! A server with many tenants replayed hundreds of installs on every boot,
//! each running the whole installer again, once more for every app that
//! depends on it.

use super::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use temper_server::registry_bootstrap::restore_registry_from_turso;
use temper_store_turso::TursoEventStore;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

/// A spec for the entity type `name`, with one move.
fn ioa(name: &str, moves: &[&str]) -> String {
    let actions: String = moves
        .iter()
        .map(|action| {
            format!(
                "\n[[action]]\nname = \"{action}\"\nkind = \"input\"\nfrom = [\"Ready\"]\nto = \"Ready\"\neffect = [\"count += 1\"]\n"
            )
        })
        .collect();
    format!(
        "[automaton]\nname = \"{name}\"\nstates = [\"Ready\"]\ninitial = \"Ready\"\n\
         allow_indefinite_states = [\"Ready\"]\n\n[[state]]\nname = \"count\"\n\
         type = \"counter\"\ninitial = 0\n{actions}"
    )
}

/// A CSDL in `namespace` declaring `name` in the set `<name>s`.
fn csdl(namespace: &str, name: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="{namespace}" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
      <EntityContainer Name="Container"><EntitySet Name="{name}s" EntityType="{namespace}.{name}"/></EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#
    )
}

/// The server one process runs on a database and apps directory.
struct Server {
    state: PlatformState,
    store: TursoEventStore,
}

struct Fixture {
    db_url: String,
    apps_root: PathBuf,
    tenant: String,
}

impl Fixture {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4();
        Self {
            db_url: format!("file:/tmp/temper-unchanged-replay-{id}.db"),
            apps_root: std::env::temp_dir().join(format!("temper-unchanged-replay-{id}")),
            tenant: format!("replay-{}", id.simple()),
        }
    }

    /// Write app `app`, declaring `entity` in `namespace` with `moves`, its
    /// Cedar, its cross-invariants and the apps it depends on.
    fn write_app(&self, app: &str, namespace: &str, entity: &str, moves: &[&str], deps: &[&str]) {
        let dir = self.apps_root.join(app);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        fs::create_dir_all(dir.join("policies")).expect("create policies dir");
        let deps: Vec<String> = deps.iter().map(|dep| format!("\"{dep}\"")).collect();
        fs::write(
            dir.join("app.toml"),
            format!(
                "name = \"{app}\"\ndescription = \"Replay test app\"\nversion = \"0.1.0\"\n\
                 dependencies = [{}]\n",
                deps.join(", ")
            ),
        )
        .expect("write app.toml");
        fs::write(dir.join("APP.md"), format!("# {app}\n")).expect("write APP.md");
        fs::write(
            dir.join(format!("specs/{}.ioa.toml", entity.to_lowercase())),
            ioa(entity, moves),
        )
        .expect("write spec");
        fs::write(dir.join("specs/model.csdl.xml"), csdl(namespace, entity)).expect("write CSDL");
        fs::write(
            dir.join("specs/cross-invariants.toml"),
            "version = 1\ndefault_delete_policy = \"restrict\"\n",
        )
        .expect("write cross-invariants");
        fs::write(
            dir.join(format!("policies/{app}.cedar")),
            format!("// {app}\npermit(principal, action, resource);\n"),
        )
        .expect("write policy");
    }

    /// A process on the fixture's database, restored as a boot restores a
    /// tenant before it replays the tenant's installs: its stored specs,
    /// Cedar and modules.
    async fn boot(&self) -> Server {
        let store = TursoEventStore::new(&self.db_url, None)
            .await
            .expect("open the database");
        let mut state = PlatformState::new(None);
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
        let mut registry = temper_server::registry::SpecRegistry::new();
        restore_registry_from_turso(&mut registry, &store)
            .await
            .expect("restore the registry");
        *state.registry.write().unwrap() = registry;
        temper_server::authz::try_load_and_activate_tenant_policies(&state.server, &self.tenant)
            .await
            .expect("load the tenant's Cedar");
        state
            .server
            .load_tenant_wasm_modules(&self.tenant)
            .await
            .expect("load the tenant's modules");
        state.server.rebuild_reaction_dispatcher();
        add_os_apps_dir(self.apps_root.clone());
        Server { state, store }
    }
}

impl Server {
    fn platform(&self) -> Arc<dyn temper_server::platform_store::PlatformStore> {
        self.state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.platform.clone())
            .expect("a platform store")
    }

    /// Replay every recorded install, in the order a boot does.
    async fn replay(&self) {
        let ps = self.platform();
        let mut recorded = ps.list_all_installed_apps().await.expect("list installs");
        recorded.sort();
        // As `temper serve` replays: the schema first, then each install.
        crate::os_apps::converge_installed_schemas(&self.state, &recorded).await;
        for (tenant, app) in recorded {
            restore_os_app(&self.state, &tenant, &app)
                .await
                .unwrap_or_else(|e| panic!("replaying {app}: {e}"));
        }
    }

    /// What the store holds for `tenant`: its spec rows, constraints,
    /// install records and Cedar.
    async fn stored(&self, tenant: &str) -> String {
        let ps = self.platform();
        let specs: Vec<String> = self
            .store
            .load_specs()
            .await
            .expect("load specs")
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| format!("{row:?}"))
            .collect();
        let constraints: Vec<String> = self
            .store
            .load_tenant_constraints()
            .await
            .expect("load constraints")
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| format!("{row:?}"))
            .collect();
        let mut records = Vec::new();
        for (record_tenant, app) in ps.list_all_installed_apps().await.expect("list installs") {
            if record_tenant == tenant {
                records.push(format!("{:?}", ps.get_installed_app(tenant, &app).await));
            }
        }
        let text: Vec<(String, String)> = self
            .store
            .load_tenant_policies()
            .await
            .expect("load the Cedar text")
            .into_iter()
            .filter(|(policy_tenant, _)| policy_tenant == tenant)
            .collect();
        let rows = self
            .state
            .server
            .policy_store()
            .expect("a policy store")
            .load_policies_for_tenant(tenant)
            .await
            .expect("load the Cedar rows");
        format!("{specs:#?}\n{constraints:#?}\n{records:#?}\n{text:#?}\n{rows:#?}")
    }

    /// What `tenant` serves: each type's source and whether it is verified,
    /// its entity sets, and its Cedar.
    fn serving(&self, tenant: &str) -> String {
        let registry = self.state.registry.read().unwrap();
        let config = registry
            .get_tenant(&TenantId::new(tenant))
            .expect("the tenant is registered");
        let types: Vec<(String, String, bool)> = config
            .entities
            .iter()
            .map(|(name, spec)| {
                let passed = registry
                    .get_verification_status(&TenantId::new(tenant), name)
                    .is_some_and(|status| status.is_passed());
                (name.clone(), spec.ioa_source.clone(), passed)
            })
            .collect();
        let sets: Vec<(String, String)> = config
            .entity_set_map
            .iter()
            .map(|(set, entity)| (set.clone(), entity.clone()))
            .collect();
        let cedar = self
            .state
            .server
            .tenant_policies
            .read()
            .expect("Cedar text lock")
            .get(tenant)
            .cloned()
            .unwrap_or_default();
        let cedar: std::collections::BTreeSet<&str> = cedar
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        format!("{types:#?}\n{sets:#?}\n{cedar:#?}")
    }
}

/// The apps the installer ran for, as its "Installed os-app" line names them.
#[derive(Clone, Default)]
struct Installs(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> Layer<S> for Installs {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        struct Message(Option<String>);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = Some(format!("{value:?}"));
                }
            }
        }
        let mut message = Message(None);
        event.record(&mut message);
        if let Some(app) = message
            .0
            .as_deref()
            .and_then(|text| text.strip_prefix("Installed os-app '"))
            .and_then(|rest| rest.split('\'').next())
        {
            self.0.lock().unwrap().push(app.to_string());
        }
    }
}

impl Installs {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

#[tokio::test]
async fn unchanged_replay_registers_and_installs_nothing() {
    let fx = Fixture::new();
    let (base, top) = (format!("base-{}", fx.tenant), format!("top-{}", fx.tenant));
    fx.write_app(&base, "Test.Base", "Shelf", &["Stock"], &[]);
    fx.write_app(&top, "Test.Top", "Order", &["Place"], &[&base]);

    let installs = Installs::default();
    let _logs = tracing::subscriber::set_default(Registry::default().with(installs.clone()));

    let first = fx.boot().await;
    install_os_app(&first.state, &fx.tenant, &top)
        .await
        .expect("install the app and its dependency");
    let served = first.serving(&fx.tenant);
    drop(first);
    installs.take();

    let restarted = fx.boot().await;
    let stored = restarted.stored(&fx.tenant).await;
    restarted.replay().await;
    let reinstalled = installs.take();
    assert!(
        reinstalled.is_empty(),
        "a boot installed again {} times apps whose installs had not changed: {reinstalled:?}",
        reinstalled.len()
    );
    assert_eq!(
        restarted.stored(&fx.tenant).await,
        stored,
        "replaying installs that had not changed rewrote the store"
    );
    assert_eq!(
        restarted.serving(&fx.tenant),
        served,
        "after the replay the tenant serves what it served before the restart"
    );
}

#[tokio::test]
async fn unchanged_replay_installs_a_bundle_that_changed_since_its_install() {
    let fx = Fixture::new();
    let (base, top) = (format!("base-{}", fx.tenant), format!("top-{}", fx.tenant));
    fx.write_app(&base, "Test.Base", "Shelf", &["Stock"], &[]);
    fx.write_app(&top, "Test.Top", "Order", &["Place"], &[&base]);
    let first = fx.boot().await;
    install_os_app(&first.state, &fx.tenant, &top)
        .await
        .expect("install the app and its dependency");
    drop(first);

    // The app's files change on disk while the server is down.
    fx.write_app(&top, "Test.Top", "Order", &["Place", "Cancel"], &[&base]);
    let installs = Installs::default();
    let _logs = tracing::subscriber::set_default(Registry::default().with(installs.clone()));
    let restarted = fx.boot().await;
    restarted.replay().await;

    assert!(
        installs.take().contains(&top),
        "the changed app is installed again"
    );
    let registry = restarted.state.registry.read().unwrap();
    let order = registry
        .get_spec(&TenantId::new(&fx.tenant), "Order")
        .expect("Order is registered");
    assert!(
        order.ioa_source.contains("Cancel"),
        "the tenant serves the changed spec"
    );
}
