//! A refused install cannot replace or delete the tenant's saved constraints.

use super::*;

const ROLE: &str = r#"
[automaton]
name = "Role"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
principal_record = true
principal_record_set = "roles"

[[state]]
name = "roles"
type = "list"
initial = []

[[action]]
name = "Touch"
from = ["Active"]
"#;

const CLASH: &str = r#"
[automaton]
name = "Role_roles"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
principal_record = true

[[action]]
name = "Touch"
from = ["Active"]
"#;

const CONSTRAINTS: &str = r#"
[[invariant]]
name = "OriginalRule"
kind = "hard"
on = "Order.*"
assert = 'related(Order, OrderId).status in ["Draft"]'
"#;

async fn install_status_type(state: &ServerState) {
    // Directory filenames normalize away underscores. Seed the status type
    // by its registered name, as an app install can, then merge a Role set.
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            temper_spec::csdl::parse_csdl(CSDL_XML).unwrap(),
            CSDL_XML.into(),
            &[("Role_roles", CLASH)],
            None,
            true,
        )
        .unwrap();
    state
        .upsert_spec_source("default", "Role_roles", CLASH, CSDL_XML)
        .await
        .unwrap();
    let sources = std::collections::BTreeMap::from([("Role_roles".into(), CLASH.into())]);
    assert_eq!(
        state
            .commit_spec_sources("default", &sources, CSDL_XML, &[])
            .await
            .unwrap(),
        Some(true)
    );
}

async fn refused_directory_install(replacement: Option<&str>) {
    let db = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", db.path().join("temper.db").display());
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let state = server(
        StorageStack::from_turso(store.clone()),
        SpecRegistry::new(),
        "collision",
    );
    let original = specs_dir(&[("order.ioa.toml", ORDER_V1)]);
    std::fs::write(original.path().join("cross-invariants.toml"), CONSTRAINTS).unwrap(); // determinism-ok: test fixture
    let installed = load_dir(&state, original.path()).await;
    assert_eq!(installed["committed"], true, "{installed}");
    let saved = store.load_tenant_constraints().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].cross_invariants_toml, CONSTRAINTS);

    install_status_type(&state).await;

    let rejected = specs_dir(&[("role.ioa.toml", ROLE)]);
    if let Some(source) = replacement {
        std::fs::write(rejected.path().join("cross-invariants.toml"), source).unwrap(); // determinism-ok: test fixture
    }
    let body = serde_json::json!({
        "tenant": "default", "specs_dir": rejected.path().to_str().unwrap(), "merge": true,
    });
    let (status, text) = send(
        &state,
        Request::post("/api/specs/load-dir")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("principal.Role_roles"), "{text}");
    let after = store.load_tenant_constraints().await.unwrap();
    assert_eq!(after.len(), 1, "refusal must not delete saved constraints");
    assert_eq!(
        after[0].cross_invariants_toml,
        saved[0].cross_invariants_toml
    );
    assert_eq!(
        after[0].version, saved[0].version,
        "refusal must not write constraints"
    );
    let rejected_sources = std::collections::BTreeMap::from([("Role".into(), ROLE.into())]);
    assert_eq!(
        state
            .commit_spec_sources("default", &rejected_sources, CSDL_XML, &[])
            .await
            .unwrap(),
        Some(false),
        "refusal must leave no spec version to commit"
    );
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_spec(&TenantId::default(), "Role")
            .is_none()
    );
    drop(state);
    drop(store);

    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut registry = SpecRegistry::new();
    restore_registry_from_turso(&mut registry, &store)
        .await
        .unwrap();
    let config = registry.get_tenant(&TenantId::default()).unwrap();
    assert_eq!(config.cross_invariants_source.as_deref(), Some(CONSTRAINTS));
    assert_eq!(config.entities["Role_roles"].ioa_source, CLASH);
    assert_eq!(config.entities["Order"].ioa_source, ORDER_V1);
    assert!(!config.entities.contains_key("Role"));
}

#[tokio::test]
async fn rejected_load_dir_preserves_constraints_when_none_are_supplied() {
    refused_directory_install(None).await;
}

#[tokio::test]
async fn rejected_load_dir_preserves_constraints_when_replacements_are_supplied() {
    let replacement = CONSTRAINTS.replace("OriginalRule", "RejectedRule");
    refused_directory_install(Some(&replacement)).await;
}
