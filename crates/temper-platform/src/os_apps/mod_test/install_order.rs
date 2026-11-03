//! A record moved while an install of a spec and module pair waits on storage
//! never meets a spec whose integration its module cannot answer: a new
//! version's module is in before its spec, and a version put back has its spec
//! back before its module (`Swap`).
use super::*;
use std::sync::Arc;
use temper_store_turso::TursoEventStore;

#[path = "install_order/storage.rs"]
mod storage;
use storage::PauseInstallStorage;

tokio::task_local! { static INSTALLER: bool; }

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Order" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Order.Job"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// A Job that a Poke moves on; `calls` adds the integration Poke runs.
fn job_spec(calls: bool) -> String {
    let mut spec = String::from(
        r#"
[automaton]
name = "Job"
states = ["Waiting", "Poked", "Answered", "Broken"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Poked", "Answered", "Broken"]
[[action]]
name = "Answered"
kind = "input"
from = ["Poked"]
to = "Answered"
[[action]]
name = "Refused"
kind = "input"
from = ["Poked"]
to = "Broken"
"#,
    );
    spec.push_str(if calls {
        r#"
[[action]]
name = "Poke"
from = ["Waiting"]
to = "Poked"
[[action.triggers]]
name = "ask"
kind = "wasm"
module = "answer"
on_success = "Answered"
on_failure = "Refused"
"#
    } else {
        r#"
[[action]]
name = "Poke"
from = ["Waiting"]
to = "Poked"
"#
    });
    spec
}

/// A module that answers every call, and one that refuses every call: the
/// stand-in for a version that does not know what a newer spec asks.
fn answering_module() -> Vec<u8> {
    fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../temper-wasm/tests/fixtures/sdk_context_reader.wasm"),
    )
    .expect("read the context reader fixture")
}
// A valid module whose invocation traps, so the declared on_failure runs.
const REFUSING_MODULE: &[u8] = br#"(module
    (memory (export "memory") 1)
    (func (export "run") (param i32 i32) (result i32) unreachable)
)"#;

/// Write one version of the app: its Job spec and its `answer` module.
fn write_version(dir: &std::path::Path, app: &str, calls: bool, module: &[u8]) {
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::create_dir_all(dir.join("wasm/answer")).expect("create the module dir");
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
name = "answer"
criticality = "app-required"
startup_loading = "lazy"
"#
        ),
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Install order").expect("write APP.md");
    fs::write(dir.join("specs/model.csdl.xml"), CSDL).expect("write csdl");
    fs::write(dir.join("specs/job.ioa.toml"), job_spec(calls)).expect("write spec");
    fs::write(dir.join("wasm/answer/answer.wasm"), module).expect("write module");
}

/// Where a Poke on a new record ends: Poked when its spec runs nothing, or
/// where the integration's answer takes it.
async fn poke(state: &PlatformState, tenant: &TenantId, id: &str) -> String {
    state
        .server
        .get_or_create_tenant_entity(tenant, "Job", id, serde_json::json!({}))
        .await
        .expect("create record");
    let response = state
        .server
        .dispatch_tenant_action(
            tenant,
            "Job",
            id,
            "Poke",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch Poke");
    assert!(response.success, "Poke failed: {:?}", response.error);
    // Action-local WASM triggers are emitted in the dispatch response under
    // their synthesized name; wait for the callback only when Poke ran one.
    if !response
        .custom_effects
        .iter()
        .any(|effect| effect == "__trigger__:Poke:ask")
    {
        return response.state.status;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let record = state
                .server
                .get_tenant_entity_state(tenant, "Job", id)
                .await
                .expect("read record");
            if record.state.status != "Poked" {
                return record.state.status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the integration must answer")
}

/// Install `to` over `from` in the order `swap` names, poking a new record
/// at every storage call the install waits on; none may break. Returns how
/// many times it waited.
async fn install_poking(name: &str, from: (bool, &[u8]), to: (bool, &[u8]), swap: Swap) -> usize {
    let plan = OsAppInstallPlan {
        specs: true,
        policies: true,
        wasm: true,
        content: false,
        seed: false,
    };
    let root = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
    // The global catalog retains the first directory registered for each app.
    // Use this test's distinct name so the tests never share a catalog entry.
    let dir = root.join(name);
    write_version(&dir, name, from.0, from.1);
    let store = TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
        .await
        .expect("open turso store");
    let mut state = PlatformState::new(None);
    state.server.data_dir = root.join("data");
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
    let tenant = TenantId::new(name);
    state
        .server
        .start_runtime()
        .await
        .expect("first start")
        .await
        .expect("boot walk");
    add_os_apps_dir(root.clone());
    install_os_app_with_plan(&state, tenant.as_str(), name, plan, InstallOrigin::Request)
        .await
        .expect("install the version before");
    assert_ne!(poke(&state, &tenant, "before").await, "Broken");

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut stack = state
        .server
        .storage_stack
        .as_ref()
        .expect("storage stack")
        .as_ref()
        .clone();
    let paused = Arc::new(PauseInstallStorage {
        inner: stack.turso.take().expect("turso provider"),
        platform: stack.platform.take().expect("platform store"),
        tenant: tenant.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });
    stack.turso = Some(paused.clone());
    stack.platform = Some(paused);
    state.server.set_storage_stack(stack);
    write_version(&dir, name, to.0, to.1);
    let install = INSTALLER.scope(
        true,
        install_os_app_swapping(
            &state,
            tenant.as_str(),
            name,
            plan,
            InstallOrigin::Request,
            swap,
        ),
    );
    tokio::pin!(install);
    let mut waits = 0;
    loop {
        tokio::select! {
            _ = entered.notified() => {},
            result = &mut install => {
                result.expect("install the version after");
                break;
            }
        }
        waits += 1;
        let status = poke(&state, &tenant, &format!("during-{waits}")).await;
        assert_ne!(
            status, "Broken",
            "a record moved while the install waited on storage ({waits}) met a spec its module cannot answer"
        );
        release.add_permits(1);
    }
    assert_ne!(poke(&state, &tenant, "after").await, "Broken");
    eprintln!("{name}: exercised {waits} storage waits");
    fs::remove_dir_all(root).expect("remove temp dir");
    waits
}

/// A new version whose spec starts calling a module: the module is in before
/// the spec that calls it.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_version_has_its_module_in_before_its_spec() {
    let answering = answering_module();
    let waits = install_poking(
        "install-order-new",
        (false, REFUSING_MODULE),
        (true, &answering),
        Swap::ModulesFirst,
    )
    .await;
    assert!(
        waits > 0,
        "the install never waited on storage, so nothing was tested"
    );
}

/// Putting back a version whose module cannot answer the spec it replaces:
/// the spec goes back before the module.
#[tokio::test(flavor = "multi_thread")]
async fn a_version_put_back_has_its_spec_back_before_its_module() {
    let answering = answering_module();
    let waits = install_poking(
        "install-order-back",
        (true, &answering),
        (false, REFUSING_MODULE),
        Swap::SpecsFirst,
    )
    .await;
    assert!(
        waits > 0,
        "the install never waited on storage, so nothing was tested"
    );
}
