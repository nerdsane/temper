use super::{cache_platform_secret_if_present, ioa_sources_requiring_background_verification};
use std::collections::{BTreeMap, HashMap};
use temper_server::secrets::vault::SecretsVault;

fn test_vault() -> SecretsVault {
    SecretsVault::new(&[0x42; 32])
}

#[test]
fn cache_platform_secret_if_present_stores_present_value() {
    let vault = test_vault();

    cache_platform_secret_if_present(&vault, "exa_api_key", Some("exa-live".to_string()));

    assert_eq!(
        vault.get_platform_secret("exa_api_key"),
        Some("exa-live".to_string())
    );
}

#[test]
fn cache_platform_secret_if_present_skips_missing_value() {
    let vault = test_vault();

    cache_platform_secret_if_present(&vault, "exa_api_key", None);

    assert_eq!(vault.get_platform_secret("exa_api_key"), None);
}

#[test]
fn background_verification_skips_cached_verified_hashes() {
    let mut ioa_sources = HashMap::new();
    ioa_sources.insert(
        "Issue".to_string(),
        "[automaton]\nname = \"Issue\"\n".to_string(),
    );
    ioa_sources.insert(
        "Task".to_string(),
        "[automaton]\nname = \"Task\"\n".to_string(),
    );

    let mut verification_cache = BTreeMap::new();
    verification_cache.insert(
        "Issue".to_string(),
        (
            temper_store_turso::spec_content_hash(ioa_sources.get("Issue").expect("Issue source")),
            true,
        ),
    );
    verification_cache.insert(
        "Task".to_string(),
        (
            temper_store_turso::spec_content_hash("different source"),
            true,
        ),
    );

    let pending = ioa_sources_requiring_background_verification(ioa_sources, &verification_cache);

    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key("Task"));
}

#[tokio::test]
async fn app_specs_are_committed_once_background_verification_passes() {
    use temper_platform::state::PlatformState;
    use temper_server::storage::StorageStack;
    use temper_store_turso::TursoEventStore;

    let dir = tempfile::tempdir().expect("temp dir");
    let specs = dir.path().join("specs");
    std::fs::create_dir_all(&specs).expect("specs dir"); // determinism-ok: test fixture
    std::fs::write(
        specs.join("model.csdl.xml"),
        include_str!("../../../../test-fixtures/specs/model.csdl.xml"),
    )
    .expect("write csdl"); // determinism-ok: test fixture
    std::fs::write(
        specs.join("order.ioa.toml"),
        include_str!("../../../../test-fixtures/specs/order.ioa.toml"),
    )
    .expect("write ioa"); // determinism-ok: test fixture
    let specs_dir = specs.to_str().expect("utf-8 path");

    // What `temper serve --app app=DIR` does at boot: register, persist, verify.
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("temper.db").display()),
        None,
    )
    .await
    .expect("turso store");
    let mut state = PlatformState::new(None);
    let loaded = {
        let mut registry = state.registry.write().unwrap();
        super::loader::load_into_registry(&mut registry, specs_dir, "app").expect("load app")
    };
    super::storage::upsert_loaded_specs_to_turso(&store, "app", &loaded)
        .await
        .expect("persist app specs");
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));

    super::spawn_background_verification(&state, specs_dir, "app")
        .await
        .expect("verification spawned")
        .await
        .expect("commit task");

    let restorable: Vec<String> = store
        .load_specs()
        .await
        .expect("load specs")
        .into_iter()
        .filter(|row| row.tenant == "app")
        .map(|row| row.entity_type)
        .collect();
    assert_eq!(
        restorable,
        vec!["Order".to_string()],
        "a verified --app install must be committed so it comes back without --app"
    );
}
