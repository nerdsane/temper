//! A lost action param is refused before an install writes, and a stored spec
//! that has one still restores.

use super::*;
use temper_server::platform_store::PlatformStore;
use temper_server::registry::SpecRegistry;

const LOST: &str = r#"[automaton]
name = "Order"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
allow_indefinite_states = ["Ready"]

[[state]]
name = "urgent"
type = "bool"
initial = false

[[action]]
name = "Mark"
kind = "input"
from = ["Ready"]
to = "Ready"
params = [{ name = "urgent", type = "bool" }]
"#;

#[tokio::test]
async fn a_lost_param_is_refused_before_install_writes() {
    let fx = Fixture::new("lost-params").await;
    let app = format!("lost-params-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, ORDER_CSDL, None);
    let source_path = fx.apps_root.join(&app).join("specs/order.ioa.toml");
    fs::write(&source_path, LOST).unwrap();
    let error = fx.install("lost-tenant", &app).await.unwrap_err();
    for name in ["Mark", "urgent = params.urgent"] {
        assert!(error.contains(name), "{error}");
    }
    assert!(fx.store.load_specs().await.unwrap().is_empty());

    let kept = LOST.replace(
        "params = [{ name = \"urgent\", type = \"bool\" }]",
        "params = [{ name = \"urgent\", type = \"bool\" }]\neffect = [\"urgent = params.urgent\"]",
    );
    fs::write(&source_path, &kept).unwrap();
    fx.install("lost-tenant", &app)
        .await
        .expect("a param the action stores installs");
}

#[tokio::test]
async fn a_stored_spec_with_a_lost_param_still_restores() {
    let fx = Fixture::new("restore-lost-params").await;
    PlatformStore::upsert_spec(
        &fx.store,
        "lost-tenant",
        "Order",
        LOST,
        ORDER_CSDL,
        &temper_store_turso::spec_content_hash(LOST),
    )
    .await
    .unwrap();
    PlatformStore::commit_specs(&fx.store, "lost-tenant")
        .await
        .unwrap();
    let mut registry = SpecRegistry::new();
    let restored = temper_server::registry_bootstrap::restore_registry_from_platform_store(
        &mut registry,
        &fx.store,
    )
    .await
    .unwrap();
    assert_eq!(restored, 1, "boot restores the old spec");
}
