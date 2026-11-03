//! An install replaces the installing app's own contribution to the tenant's
//! schema: what the app stopped declaring leaves, other apps' declarations
//! stay, a type whose spec is still registered keeps its declaration, and a
//! restart does not bring a dropped type back.

use super::*;
use std::path::PathBuf;

use temper_server::registry_bootstrap::restore_registry_from_turso;
use temper_store_turso::TursoEventStore;

/// A spec for the entity type `name`.
fn ioa(name: &str) -> String {
    format!(
        r#"[automaton]
name = "{name}"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["count += 1"]
"#
    )
}

/// A CSDL in `namespace` declaring `types`, each in a set named `<type>s`.
fn csdl(namespace: &str, types: &[&str]) -> String {
    let entity_types: String = types
        .iter()
        .map(|name| {
            format!(
                r#"<EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>"#
            )
        })
        .collect();
    let sets: String = types
        .iter()
        .map(|name| format!(r#"<EntitySet Name="{name}s" EntityType="{namespace}.{name}"/>"#))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="{namespace}" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      {entity_types}
      <EntityContainer Name="Container">{sets}</EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#
    )
}

struct Fixture {
    state: PlatformState,
    store: TursoEventStore,
    db_url: String,
    apps_root: PathBuf,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let id = uuid::Uuid::new_v4();
        let db_url = format!("file:/tmp/temper-own-schema-{label}-{id}.db");
        let apps_root = std::env::temp_dir().join(format!("temper-own-schema-{label}-{id}"));
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("open the own schema database");
        let mut state = PlatformState::new(None);
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
        Self {
            state,
            store,
            db_url,
            apps_root,
        }
    }

    /// Write (or rewrite) app `app`: a CSDL in `namespace` declaring `types`
    /// and a spec for each of `specs`.
    fn write_app(&self, app: &str, namespace: &str, types: &[&str], specs: &[&str]) {
        let app_dir = self.apps_root.join(app);
        let _ = fs::remove_dir_all(&app_dir);
        fs::create_dir_all(app_dir.join("specs")).expect("create specs dir");
        fs::write(
            app_dir.join("app.toml"),
            format!(
                "name = \"{app}\"\ndescription = \"Own schema test app\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("write app.toml");
        fs::write(app_dir.join("APP.md"), format!("# {app}\n\nTest app.\n")).expect("write APP.md");
        for spec in specs {
            fs::write(
                app_dir.join(format!("specs/{}.ioa.toml", spec.to_lowercase())),
                ioa(spec),
            )
            .expect("write spec");
        }
        fs::write(app_dir.join("specs/model.csdl.xml"), csdl(namespace, types))
            .expect("write CSDL");
    }

    async fn install(&self, tenant: &str, app: &str) -> InstallResult {
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, tenant, app)
            .await
            .unwrap_or_else(|e| panic!("installing {app} should succeed: {e}"))
    }

    /// The tenant's CSDL as the registry serves it: (entity type names,
    /// entity set name -> its namespace-qualified type).
    fn schema(&self, tenant: &str) -> (Vec<String>, Vec<(String, String)>) {
        let registry = self.state.registry.read().unwrap();
        let config = registry
            .get_tenant(&TenantId::new(tenant))
            .expect("the tenant is registered");
        let mut types = Vec::new();
        let mut sets = Vec::new();
        for schema in &config.csdl.schemas {
            types.extend(
                schema
                    .entity_types
                    .iter()
                    .map(|t| format!("{}.{}", schema.namespace, t.name)),
            );
            for container in &schema.entity_containers {
                sets.extend(
                    container
                        .entity_sets
                        .iter()
                        .map(|s| (s.name.clone(), s.entity_type.clone())),
                );
            }
        }
        (types, sets)
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// Reinstalling an app whose new version stopped declaring a type removes the
/// type and its entity set from the tenant's schema.
#[tokio::test]
async fn a_reinstall_drops_what_the_app_no_longer_declares() {
    let fx = Fixture::new("drops").await;
    let (tenant, app) = (unique("own-tenant"), unique("own-app"));
    fx.write_app(&app, "Test.One", &["Counter", "Extra"], &["Counter"]);
    fx.install(&tenant, &app).await;
    let (types, _) = fx.schema(&tenant);
    assert!(types.contains(&"Test.One.Extra".to_string()), "{types:?}");

    fx.write_app(&app, "Test.One", &["Counter"], &["Counter"]);
    fx.install(&tenant, &app).await;

    let (types, sets) = fx.schema(&tenant);
    assert!(
        !types.contains(&"Test.One.Extra".to_string()),
        "the reinstalled app no longer declares Extra, but the tenant schema still has the type: {types:?}"
    );
    assert!(
        !sets.iter().any(|(name, _)| name == "Extras"),
        "the reinstalled app no longer declares Extras, but the tenant schema still has the set: {sets:?}"
    );
    assert!(types.contains(&"Test.One.Counter".to_string()), "{types:?}");
}

/// An app's old declaration of a name another app declares in its own
/// namespace goes, leaving one entity set with that name: the other app's.
#[tokio::test]
async fn a_dropped_declaration_leaves_one_entity_set_with_the_name() {
    let fx = Fixture::new("paw").await;
    let tenant = unique("own-tenant");
    let (pm, agent) = (unique("own-pm"), unique("own-agent"));
    fx.write_app(&pm, "Test.PM", &["Project"], &["Project"]);
    fx.write_app(&agent, "Test.Agent", &["Project"], &["Project"]);
    // Both are held from before: the pair a request would refuse.
    fx.install(&tenant, &pm).await;
    fx.store
        .record_installed_app(&tenant, &agent)
        .await
        .expect("record the second owner");
    add_os_apps_dir(fx.apps_root.clone());
    restore_os_app(&fx.state, &tenant, &agent)
        .await
        .expect("the replay restores the second owner");
    let (_, sets) = fx.schema(&tenant);
    assert_eq!(
        sets.iter().filter(|(name, _)| name == "Projects").count(),
        2,
        "the setup should hold two Projects sets: {sets:?}"
    );

    // The first app's new version drops Project and declares Counter.
    fx.write_app(&pm, "Test.PM", &["Counter"], &["Counter"]);
    fx.install(&tenant, &pm).await;

    let (_, sets) = fx.schema(&tenant);
    let projects: Vec<_> = sets.iter().filter(|(name, _)| name == "Projects").collect();
    assert_eq!(
        projects.len(),
        1,
        "exactly one Projects set should remain: {sets:?}"
    );
    assert_eq!(
        projects[0].1, "Test.Agent.Project",
        "the remaining Projects set should be the other app's"
    );
}

/// A type no installed app declares any more, whose spec is still registered,
/// keeps its declaration so its records stay readable, and the install says so.
#[tokio::test]
async fn a_type_whose_spec_is_registered_keeps_its_declaration() {
    let fx = Fixture::new("keeps").await;
    let (tenant, app) = (unique("own-tenant"), unique("own-app"));
    fx.write_app(
        &app,
        "Test.One",
        &["Counter", "Widget"],
        &["Counter", "Widget"],
    );
    fx.install(&tenant, &app).await;

    fx.write_app(&app, "Test.One", &["Counter"], &["Counter"]);
    let result = fx.install(&tenant, &app).await;

    let (types, sets) = fx.schema(&tenant);
    assert!(
        types.contains(&"Test.One.Widget".to_string())
            && sets.iter().any(|(name, _)| name == "Widgets"),
        "Widget's spec is registered, so its declaration and set should stay: {types:?} {sets:?}"
    );
    assert!(
        result.warnings.iter().any(|w| w.check == "own_schema"
            && w.message.contains("entity type Widget")
            && w.message.contains("its spec is registered")),
        "the install should warn that Widget was kept: {:?}",
        result.warnings
    );
}

/// After a restart (registry restored from the stored rows, then app
/// recovery), a type the app dropped does not come back.
#[tokio::test]
async fn a_restart_does_not_bring_a_dropped_type_back() {
    let fx = Fixture::new("restart").await;
    let (tenant, first, second) = (unique("own-tenant"), unique("own-a"), unique("own-b"));
    fx.write_app(&first, "Test.One", &["Counter", "Extra"], &["Counter"]);
    fx.write_app(&second, "Test.Two", &["Other"], &["Other"]);
    fx.install(&tenant, &first).await;
    fx.install(&tenant, &second).await;
    // The first app's new version drops Extra. The second app's rows still
    // hold the schema from before.
    fx.write_app(&first, "Test.One", &["Counter"], &["Counter"]);
    fx.install(&tenant, &first).await;

    let restarted = TursoEventStore::new(&fx.db_url, None)
        .await
        .expect("reopen the database");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(restarted.clone()));
    {
        let mut restored = temper_server::registry::SpecRegistry::new();
        restore_registry_from_turso(&mut restored, &restarted)
            .await
            .expect("restore the registry");
        *state.registry.write().unwrap() = restored;
    }
    add_os_apps_dir(fx.apps_root.clone());
    crate::recovery::restore_installed_apps(&state, &restarted).await;

    let registry = state.registry.read().unwrap();
    let config = registry
        .get_tenant(&TenantId::new(&tenant))
        .expect("the tenant is restored");
    let declares = |name: &str| {
        config
            .csdl
            .schemas
            .iter()
            .any(|s| s.entity_types.iter().any(|t| t.name == name))
    };
    assert!(
        !declares("Extra") && !config.entity_set_map.contains_key("Extras"),
        "after a restart the tenant schema declares Extra again, which the app dropped: {:?}",
        config.entity_set_map
    );
    assert!(
        declares("Counter") && declares("Other"),
        "the apps' current declarations should be restored: {:?}",
        config.entity_set_map
    );
}

/// A restart while an installed app's files cannot be read keeps that app's
/// declarations: the boot neither rebuilds the schema without it nor drops
/// its types, even one that has no spec to keep it.
#[tokio::test]
async fn a_restart_keeps_the_schema_of_an_app_it_cannot_read() {
    restart_keeps_unreadable_schema(false).await;
}

#[tokio::test]
async fn a_restart_keeps_the_schema_of_a_written_app_whose_manifest_cannot_be_read() {
    restart_keeps_unreadable_schema(true).await;
}

async fn restart_keeps_unreadable_schema(written: bool) {
    let fx = Fixture::new("unreadable").await;
    let (tenant, first, second) = (unique("own-tenant"), unique("own-a"), unique("own-b"));
    fx.write_app(&first, "Test.Shared", &["Counter"], &["Counter"]);
    fx.write_app(&second, "Test.Shared", &["Other", "Note"], &["Other"]);
    fx.install(&tenant, &first).await;
    fx.install(&tenant, &second).await;

    let restarted = TursoEventStore::new(&fx.db_url, None)
        .await
        .expect("reopen the database");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(restarted.clone()));
    {
        let mut restored = temper_server::registry::SpecRegistry::new();
        restore_registry_from_turso(&mut restored, &restarted)
            .await
            .expect("restore the registry");
        *state.registry.write().unwrap() = restored;
    }
    add_os_apps_dir(fx.apps_root.clone());
    let second_dir = fx.apps_root.join(&second);
    if written {
        let source = AppFileSource::written(
            second_dir.clone(),
            ["app.toml", "APP.md"].map(str::to_string),
        );
        add_tenant_written_apps(
            &tenant,
            fx.apps_root.clone(),
            vec![(second.clone(), source)],
        );
        // Its recorded files no longer declare a schema, but an unreadable
        // manifest means the app cannot be treated as intentionally empty.
        // Unrecorded CSDL on disk and empty listings must not change that.
        app_files_seam::list_empty_under(&second_dir);
        app_files_seam::unreadable_under(&second_dir, "app.toml");
    } else {
        // The catalog lists the second app, but its files are gone.
        fs::remove_dir_all(second_dir).expect("remove the second app's files");
    }

    // Recovery, then the replay of the first app as a server boot runs it.
    crate::recovery::restore_installed_apps(&state, &restarted).await;
    restore_os_app(&state, &tenant, &first)
        .await
        .expect("the first app replays");

    let registry = state.registry.read().unwrap();
    let config = registry
        .get_tenant(&TenantId::new(&tenant))
        .expect("the tenant is restored");
    for set in ["Counters", "Others", "Notes"] {
        assert!(
            config.entity_set_map.contains_key(set),
            "a boot with an unreadable app should keep serving {set}: {:?}",
            config.entity_set_map
        );
    }
}
