//! Installed specs across restarts.
//!
//! A failed reinstall never costs a type its last working version.
//!
//! Install v1 of a type (verified, committed), create a record, then install a
//! v2 that fails verification. After a restart, v1's spec and record answer;
//! the failed v2 is discarded, and a type that never committed stays out. Run
//! against Turso always, and against PostgreSQL when `TEMPER_TEST_POSTGRES_URL`
//! names a disposable database. A replace-mode load-dir that drops a type
//! removes it durably.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres_store, restore_registry_from_turso,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_V1: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Order v2 that fails verification: `Complete` reaches Done with no items.
const ORDER_V2_FAILING: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = 0

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
effect = ["items += 1"]

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"

[[invariant]]
name = "DoneHasItems"
assert = "status in ['Done'] => items > 0"
"#;

const GHOST_IOA: &str =
    "[automaton]\nname = \"Ghost\"\nstates = [\"Draft\"]\ninitial = \"Draft\"\n";

/// Test-only blanket permit: this file checks persistence, not authorization.
const PERMIT_ALL: &str = "permit(principal, action, resource);";

fn server(storage: StorageStack, registry: SpecRegistry, name: &str) -> ServerState {
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(storage);
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("test policy parses");
    state
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, String) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: "operator".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "spec-restart-test".to_string(),
                cause: None,
                credential: None,
            },
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Load-inline one Order version and return the stream's summary line.
async fn install_order(state: &ServerState, ioa: &str) -> serde_json::Value {
    let body = serde_json::json!({
        "tenant": "default",
        "specs": { "model.csdl.xml": CSDL_XML, "order.ioa.toml": ioa },
    });
    let (status, text) = send(
        state,
        Request::post("/api/specs/load-inline")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| line["type"] == "summary")
        .expect("load-inline stream ends with a summary")
}

/// Run 1: install v1, create a record, install a failing v2.
async fn install_v1_then_failing_v2(state: &ServerState) {
    let v1 = install_order(state, ORDER_V1).await;
    assert_eq!(v1["committed"], true, "{v1}");
    let (status, text) = send(
        state,
        Request::post("/tdata/Orders")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"id":"o1"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    let v2 = install_order(state, ORDER_V2_FAILING).await;
    assert_eq!(v2["all_passed"], false, "{v2}");
    assert_eq!(v2["committed"], false, "{v2}");
    // A type whose install never committed (here: written, never verified).
    state
        .upsert_spec_source("default", "Ghost", GHOST_IOA, CSDL_XML)
        .await
        .unwrap();
}

/// Run 2 (after restart): v1 is the registered spec and its record answers.
async fn assert_v1_answers(state: &ServerState) {
    let source = state
        .registry
        .read()
        .unwrap()
        .get_spec(&TenantId::default(), "Order")
        .map(|spec| spec.ioa_source.clone());
    let restored = match source.as_deref() {
        None => "nothing",
        Some(ORDER_V1) => "v1",
        Some(_) => "the failed v2",
    };
    assert_eq!(
        restored, "v1",
        "the last committed version must come back after a failed reinstall"
    );
    let (status, text) = send(
        state,
        Request::get("/tdata/Orders('o1')")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_spec(&TenantId::default(), "Ghost")
            .is_none(),
        "a type that never committed must not be restored"
    );
}

#[tokio::test]
async fn failed_reinstall_keeps_last_committed_version_on_turso() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", dir.path().join("temper.db").display());

    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let state = server(
        StorageStack::from_turso(store),
        SpecRegistry::new(),
        "run-1",
    );
    install_v1_then_failing_v2(&state).await;
    drop(state);

    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut registry = SpecRegistry::new();
    restore_registry_from_turso(&mut registry, &store)
        .await
        .unwrap();
    let state = server(StorageStack::from_turso(store), registry, "run-2");
    assert_v1_answers(&state).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn failed_reinstall_keeps_last_committed_version_on_postgres() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let name = format!(
        "spec_restart_{}",
        temper_runtime::scheduler::sim_uuid().simple()
    );
    let schema = PostgresSchema::new(name).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();

    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let state = server(
        StorageStack::from_postgres(store),
        SpecRegistry::new(),
        "run-1",
    );
    install_v1_then_failing_v2(&state).await;
    drop(state);

    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let mut registry = SpecRegistry::new();
    restore_registry_from_postgres_store(&mut registry, &store)
        .await
        .unwrap();
    let state = server(StorageStack::from_postgres(store), registry, "run-2");
    assert_v1_answers(&state).await;

    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// Write a specs directory with the fixture CSDL and the given IOA files.
fn specs_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("model.csdl.xml"), CSDL_XML).unwrap(); // determinism-ok: test fixture
    for (name, source) in files {
        std::fs::write(dir.path().join(name), source).unwrap(); // determinism-ok: test fixture
    }
    dir
}

/// Load-dir in replace mode (the default) and return the summary line.
async fn load_dir(state: &ServerState, dir: &std::path::Path) -> serde_json::Value {
    let body = serde_json::json!({ "tenant": "default", "specs_dir": dir.to_str().unwrap() });
    let (status, text) = send(
        state,
        Request::post("/api/specs/load-dir")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| line["type"] == "summary")
        .expect("load-dir stream ends with a summary")
}

#[tokio::test]
async fn replace_mode_load_dir_removes_dropped_types_durably() {
    const DIRECTORY_IOA: &str = include_str!("../../../test-fixtures/specs/directory.ioa.toml");
    let db = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", db.path().join("temper.db").display());
    let both = specs_dir(&[
        ("order.ioa.toml", ORDER_V1),
        ("directory.ioa.toml", DIRECTORY_IOA),
    ]);
    let order_only = specs_dir(&[("order.ioa.toml", ORDER_V1)]);

    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let state = server(
        StorageStack::from_turso(store),
        SpecRegistry::new(),
        "run-1",
    );
    let first = load_dir(&state, both.path()).await;
    assert_eq!(first["committed"], true, "{first}");
    let second = load_dir(&state, order_only.path()).await;
    assert_eq!(second["committed"], true, "{second}");
    drop(state);

    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut registry = SpecRegistry::new();
    restore_registry_from_turso(&mut registry, &store)
        .await
        .unwrap();
    let mut restored: Vec<&str> = registry.entity_types(&TenantId::default());
    restored.sort_unstable();
    assert_eq!(
        restored,
        vec!["Order"],
        "a type a replace-mode load dropped must stay dropped after restart"
    );
}
