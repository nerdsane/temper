//! Refuse bad args before install writes, while stored specs still restore.

use super::*;
use temper_server::platform_store::PlatformStore;
use temper_server::registry::SpecRegistry;

const BAD_ARGS: &str = r#"[automaton]
name = "Order"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
allow_indefinite_states = ["Ready"]

[[action]]
name = "Diagnosed"
kind = "input"
from = ["Ready"]
to = "Ready"
[[action.triggers]]
name = "retry"
kind = "entity"
target_entity = "Order"
target_action = "Retry"
[action.triggers.resolve_target]
kind = "same_id"
[action.triggers.args]
moved_at = "'now'"
moved_via = "'the line'"

[[action]]
name = "Retry"
kind = "input"
from = ["Ready"]
to = "Ready"
"#;

#[tokio::test]
async fn trigger_args_refused_at_install_and_upgrade_before_writes() {
    let fx = Fixture::new("trigger-args").await;
    let app = format!("trigger-args-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, ORDER_CSDL, None);
    let source_path = fx.apps_root.join(&app).join("specs/order.ioa.toml");
    fs::write(&source_path, BAD_ARGS).unwrap();
    let error = fx.install("trigger-tenant", &app).await.unwrap_err();
    for name in ["retry", "Retry", "moved_at"] {
        assert!(error.contains(name), "{error}");
    }
    assert!(fx.store.load_specs().await.unwrap().is_empty());
    assert!(
        fx.state
            .registry
            .read()
            .unwrap()
            .get_tenant(&TenantId::new("trigger-tenant"))
            .is_none()
    );

    let good = BAD_ARGS.replace("moved_at = \"'now'\"\nmoved_via = \"'the line'\"", "");
    fs::write(&source_path, &good).unwrap();
    fx.install("trigger-tenant", &app)
        .await
        .expect("valid args install");
    fs::write(&source_path, BAD_ARGS).unwrap();
    fx.install("trigger-tenant", &app)
        .await
        .expect_err("bad upgrade is refused");
    let rows = fx.store.load_specs().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].ioa_source, good,
        "refusal preserves the stored spec"
    );
    let registry = fx.state.registry.read().unwrap();
    assert_eq!(
        registry
            .get_spec(&TenantId::new("trigger-tenant"), "Order")
            .unwrap()
            .ioa_source,
        good
    );
}

#[tokio::test]
async fn trigger_args_restored_from_store_still_load_and_warn() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Log(Arc::default());
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let fx = Fixture::new("restore-trigger-args").await;
    PlatformStore::upsert_spec(
        &fx.store,
        "trigger-tenant",
        "Order",
        BAD_ARGS,
        ORDER_CSDL,
        &temper_store_turso::spec_content_hash(BAD_ARGS),
    )
    .await
    .unwrap();
    PlatformStore::commit_specs(&fx.store, "trigger-tenant")
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
    let spec = registry
        .get_spec(&TenantId::new("trigger-tenant"), "Order")
        .expect("restored type serves");
    assert_eq!(spec.ioa_source, BAD_ARGS);
    assert_eq!(spec.automaton.actions[0].triggers[0].args.len(), 2);
    // The second boot phase replays the installed app through its installer.
    let app = format!("replay-trigger-args-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, ORDER_CSDL, None);
    fs::write(
        fx.apps_root.join(&app).join("specs/order.ioa.toml"),
        BAD_ARGS,
    )
    .unwrap();
    fx.store
        .record_installed_app("trigger-tenant", &app)
        .await
        .unwrap();
    *fx.state.registry.write().unwrap() = registry;
    add_os_apps_dir(fx.apps_root.clone());
    restore_os_app(&fx.state, "trigger-tenant", &app)
        .await
        .expect("app boot replay also keeps loading the spec");
    let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    for name in [
        "WARN",
        "trigger 'retry' on action 'Diagnosed'",
        "Retry",
        "moved_at",
        "moved_via",
    ] {
        assert!(
            output.contains(name),
            "{name} missing from warning: {output}"
        );
    }
    assert_eq!(
        fx.store.load_specs().await.unwrap().len(),
        1,
        "restore keeps the stored row"
    );
}
