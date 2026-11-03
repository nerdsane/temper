//! An app owns the entity types its CSDL declares and the specs that run
//! them: an install that would leave a spec without its type, or give an
//! entity set a second app, is refused; what an app declares and does not
//! run is a warning.

use super::*;
use std::path::PathBuf;

use temper_store_turso::TursoEventStore;

const COUNTER_IOA: &str = r#"[automaton]
name = "Counter"
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
"#;

/// A CSDL with the entity types `types`, each in a set named `<type>s`.
fn csdl(types: &[&str]) -> String {
    let entity_types: String = types
        .iter()
        .map(|name| {
            format!(
                r#"<EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="Status" Type="Edm.String"/></EntityType>"#
            )
        })
        .collect();
    let sets: String = types
        .iter()
        .map(|name| format!(r#"<EntitySet Name="{name}s" EntityType="Test.Owners.{name}"/>"#))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Owners" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      {entity_types}
      <EntityContainer Name="Owners">{sets}</EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#
    )
}

struct Fixture {
    state: PlatformState,
    store: TursoEventStore,
    apps_root: PathBuf,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let id = uuid::Uuid::new_v4();
        let db_url = format!("file:/tmp/temper-entity-owners-{label}-{id}.db");
        let apps_root = std::env::temp_dir().join(format!("temper-entity-owners-{label}-{id}"));
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("open the entity owners database");
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

    /// Write an app named `app` whose specs/ holds the Counter spec when
    /// `spec` is set, and a CSDL declaring `types`. Returns its directory.
    fn write_app(&self, app: &str, spec: bool, types: &[&str]) -> PathBuf {
        let app_dir = self.apps_root.join(app);
        fs::create_dir_all(app_dir.join("specs")).expect("create specs dir");
        fs::write(
            app_dir.join("app.toml"),
            format!(
                "name = \"{app}\"\ndescription = \"Entity owners test app\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("write app.toml");
        fs::write(app_dir.join("APP.md"), format!("# {app}\n\nTest app.\n")).expect("write APP.md");
        if spec {
            fs::write(app_dir.join("specs/counter.ioa.toml"), COUNTER_IOA).expect("write spec");
        }
        fs::write(app_dir.join("specs/model.csdl.xml"), csdl(types)).expect("write CSDL");
        app_dir
    }

    async fn install(&self, tenant: &str, app: &str) -> Result<InstallResult, String> {
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, tenant, app).await
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn a_spec_its_app_does_not_declare_is_refused() {
    let fx = Fixture::new("undeclared").await;
    let app = unique("owners-undeclared");
    fx.write_app(&app, true, &["Widget"]);

    let error = fx
        .install("owners-tenant", &app)
        .await
        .expect_err("a spec with no entity type in its app's CSDL is refused");
    assert!(
        error.contains("spec Counter has no entity type Counter in the app's CSDL"),
        "{error}"
    );
}

#[tokio::test]
async fn an_entity_set_another_app_owns_is_refused_naming_that_app() {
    let fx = Fixture::new("second-owner").await;
    let tenant = unique("owners-tenant");
    let first = unique("owners-first");
    let second = unique("owners-second");
    fx.write_app(&first, true, &["Counter"]);
    fx.write_app(&second, true, &["Counter"]);

    fx.install(&tenant, &first)
        .await
        .expect("the first owner installs");
    fx.install(&tenant, &first)
        .await
        .expect("an app reinstalled over itself is not a second owner");
    let error = fx
        .install(&tenant, &second)
        .await
        .expect_err("a second owner of Counters is refused");
    assert!(
        error.contains(&format!(
            "entity set Counters is already owned by app '{first}'"
        )),
        "{error}"
    );

    let other_tenant = unique("owners-other");
    fx.install(&other_tenant, &second)
        .await
        .expect("another tenant's owner is no owner here");
}

/// A boot replay restores what the tenant already runs: a second owner it
/// holds from before is logged, never refused, or the app would be dropped
/// at start. The same state refuses an install request.
#[tokio::test]
async fn a_replay_restores_a_second_owner_that_a_request_refuses() {
    let fx = Fixture::new("replay").await;
    let tenant = unique("owners-tenant");
    let held = unique("owners-held");
    let replayed = unique("owners-replayed");
    fx.write_app(&held, true, &["Counter"]);
    fx.write_app(&replayed, true, &["Counter"]);
    fx.store
        .record_installed_app(&tenant, &held)
        .await
        .expect("record the app the tenant already holds");

    add_os_apps_dir(fx.apps_root.clone());
    let error = install_os_app(&fx.state, &tenant, &replayed)
        .await
        .expect_err("a request is refused");
    assert!(
        error.contains(&format!("already owned by app '{held}'")),
        "{error}"
    );
    restore_os_app(&fx.state, &tenant, &replayed)
        .await
        .expect("a replay restores it");
}

/// Two apps a tenant holds from before both declaring one set is a standing
/// pair: upgrading either goes ahead and names both, so no running tenant
/// is broken and the pair shows in the warnings.
#[tokio::test]
async fn an_existing_pair_upgrades_with_a_warning_naming_both() {
    let fx = Fixture::new("standing").await;
    let tenant = unique("owners-tenant");
    let held = unique("owners-held");
    let upgraded = unique("owners-upgraded");
    fx.write_app(&held, true, &["Counter"]);
    fx.write_app(&upgraded, true, &["Counter"]);
    for app in [&held, &upgraded] {
        fx.store
            .record_installed_app(&tenant, app)
            .await
            .expect("record the pair the tenant holds from before");
    }

    let result = fx
        .install(&tenant, &upgraded)
        .await
        .expect("an existing pair is not refused");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.check == "entity_ownership"
                && warning.message
                    == format!(
                        "entity set Counters is declared by both '{upgraded}' and '{held}'"
                    )),
        "{:?}",
        result.warnings
    );
}

#[tokio::test]
async fn a_type_with_no_spec_and_a_shadowed_file_are_warnings() {
    let fx = Fixture::new("warnings").await;
    let app = unique("owners-warnings");
    let app_dir = fx.write_app(&app, true, &["Counter", "Note"]);
    fs::write(app_dir.join("counter.ioa.toml"), COUNTER_IOA).expect("write root spec");

    let result = fx
        .install(&unique("owners-tenant"), &app)
        .await
        .expect("warnings do not refuse");
    let messages: Vec<&str> = result
        .warnings
        .iter()
        .filter(|warning| warning.check == "entity_ownership")
        .map(|warning| warning.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "entity type Note (set Notes) has no spec in the app, so it is served as schema \
                 with no state machine"
                .to_string(),
            "specs/counter.ioa.toml is not read: counter.ioa.toml is read in its place".to_string(),
        ]
    );
}

#[tokio::test]
async fn written_installs_keep_entity_ownership_when_folders_list_empty() {
    let fx = Fixture::new("written-owners").await;
    let tenant = unique("owners-tenant");
    let first = unique("owners-first");
    let second = unique("owners-second");
    let sources = [&first, &second]
        .into_iter()
        .map(|app| {
            let dir = fx.write_app(app, true, &["Counter"]);
            let source = AppFileSource::written(
                dir,
                [
                    "app.toml",
                    "APP.md",
                    "specs/counter.ioa.toml",
                    "specs/model.csdl.xml",
                ]
                .map(str::to_string),
            );
            (app.clone(), source)
        })
        .collect();
    add_tenant_written_apps(&tenant, fx.apps_root.clone(), sources);
    app_files_seam::list_empty_under(&fx.apps_root);
    install_os_app(&fx.state, &tenant, &first)
        .await
        .expect("first written owner installs without listings");
    let error = install_os_app(&fx.state, &tenant, &second)
        .await
        .expect_err("the recorded first owner's CSDL still reserves Counters");
    assert!(
        error.contains(&format!(
            "entity set Counters is already owned by app '{first}'"
        )),
        "{error}"
    );
}
