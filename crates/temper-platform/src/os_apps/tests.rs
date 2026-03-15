use super::*;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::VerificationCascade;

#[test]
fn test_pm_specs_parse() {
    for (entity_type, ioa_source) in PM_SPECS {
        let result = automaton::parse_automaton(ioa_source);
        assert!(
            result.is_ok(),
            "PM spec {} failed to parse: {:?}",
            entity_type,
            result.err()
        );
    }
}

#[test]
fn test_pm_csdl_parses() {
    let result = parse_csdl(PM_CSDL);
    assert!(
        result.is_ok(),
        "PM CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_pm_spec_entity_names() {
    for (entity_type, ioa_source) in PM_SPECS {
        let a = automaton::parse_automaton(ioa_source).unwrap();
        assert_eq!(
            a.automaton.name, *entity_type,
            "PM spec name mismatch: expected {entity_type}, got {}",
            a.automaton.name
        );
    }
}

#[test]
fn test_pm_specs_verify() {
    for (entity_type, ioa_source) in PM_SPECS {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "PM spec {} failed verification",
            entity_type
        );
    }
}

#[test]
fn test_agent_orchestration_specs_parse() {
    for (entity_type, ioa_source) in AO_SPECS {
        let result = automaton::parse_automaton(ioa_source);
        assert!(
            result.is_ok(),
            "Agent Orchestration spec {} failed to parse: {:?}",
            entity_type,
            result.err()
        );
    }
}

#[test]
fn test_agent_orchestration_csdl_parses() {
    let result = parse_csdl(AO_CSDL);
    assert!(
        result.is_ok(),
        "Agent Orchestration CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_agent_orchestration_specs_verify() {
    for (entity_type, ioa_source) in AO_SPECS {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(30);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "Agent Orchestration spec {} failed verification",
            entity_type
        );
    }
}

#[test]
fn test_list_os_apps_returns_catalog() {
    let apps = list_os_apps();
    assert_eq!(apps.len(), 3);
    assert_eq!(apps[0].name, "project-management");
    assert_eq!(apps[0].entity_types.len(), 5);
    assert_eq!(apps[1].name, "temper-fs");
    assert_eq!(apps[1].entity_types.len(), 4);
    assert_eq!(apps[2].name, "agent-orchestration");
    assert_eq!(apps[2].entity_types.len(), 3);
}

#[test]
fn test_get_os_app_project_management() {
    let bundle = get_os_app("project-management");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 5);
    assert!(!bundle.csdl.is_empty());
    assert_eq!(bundle.cedar_policies.len(), 1);
}

#[test]
fn test_get_os_app_agent_orchestration() {
    let bundle = get_os_app("agent-orchestration");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 3);
    assert!(!bundle.csdl.is_empty());
    assert_eq!(bundle.cedar_policies.len(), 1);
}

#[test]
fn test_get_os_app_nonexistent() {
    assert!(get_os_app("nonexistent").is_none());
}

#[tokio::test]
async fn test_install_os_app_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-pm", "project-management").await;
    assert!(result.is_ok());
    let entities = result.unwrap();
    assert_eq!(entities.len(), 5);
    assert!(entities.contains(&"Issue".to_string()));
    assert!(entities.contains(&"Project".to_string()));
    assert!(entities.contains(&"Cycle".to_string()));
    assert!(entities.contains(&"Comment".to_string()));
    assert!(entities.contains(&"Label".to_string()));

    // Verify entities are in the registry.
    let registry = state.registry.read().unwrap();
    let tenant = TenantId::new("test-pm");
    assert!(registry.get_table(&tenant, "Issue").is_some());
    assert!(registry.get_table(&tenant, "Project").is_some());
    assert!(registry.get_table(&tenant, "Cycle").is_some());
    assert!(registry.get_table(&tenant, "Comment").is_some());
    assert!(registry.get_table(&tenant, "Label").is_some());
}

#[tokio::test]
async fn test_install_agent_orchestration_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-ao", "agent-orchestration").await;
    assert!(result.is_ok());
    let entities = result.unwrap();
    assert_eq!(entities.len(), 3);
    assert!(entities.contains(&"HeartbeatRun".to_string()));
    assert!(entities.contains(&"Organization".to_string()));
    assert!(entities.contains(&"BudgetLedger".to_string()));

    let registry = state.registry.read().unwrap();
    let tenant = TenantId::new("test-ao");
    assert!(registry.get_table(&tenant, "HeartbeatRun").is_some());
    assert!(registry.get_table(&tenant, "Organization").is_some());
    assert!(registry.get_table(&tenant, "BudgetLedger").is_some());
}

#[tokio::test]
async fn test_install_os_app_nonexistent_returns_error() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test", "nonexistent").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found in catalog"));
}

/// Proves the full install → persist → reboot → restore cycle.
///
/// 1. Install OS app with a real Turso-backed SQLite DB.
/// 2. Verify specs land in both registry and Turso.
/// 3. Build a fresh PlatformState (simulating restart) with the same DB.
/// 4. Restore registry from Turso.
/// 5. Verify specs survived the "restart".
#[tokio::test]
async fn test_os_app_install_survives_restart() {
    use std::sync::Arc;
    use temper_server::event_store::ServerEventStore;
    use temper_server::registry_bootstrap::restore_registry_from_turso;
    use temper_store_turso::TursoEventStore;

    // Use a unique temp file DB for this test.
    let db_path = format!("/tmp/temper-test-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    // ── Phase A: Install into a fresh state with Turso. ─────────
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state.server.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

    let result = install_os_app(&state, "test-ws", "project-management").await;
    assert!(result.is_ok(), "install failed: {:?}", result.err());
    let entities = result.unwrap();
    assert_eq!(entities.len(), 5);

    // Verify specs are in the in-memory registry.
    {
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
    }

    // Verify specs are persisted to Turso.
    let turso_ref = state
        .server
        .event_store
        .as_ref()
        .unwrap()
        .platform_turso_store()
        .unwrap();
    let rows = turso_ref.load_specs().await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.tenant == "test-ws" && r.entity_type == "Issue"),
        "Issue spec not found in Turso"
    );

    // Verify installed_apps record is in Turso.
    let installed = turso_ref.list_all_installed_apps().await.unwrap();
    assert!(
        installed.contains(&("test-ws".to_string(), "project-management".to_string())),
        "installed app record not found"
    );

    // ── Phase B: Simulate restart — fresh state, same DB. ───────
    let turso2 = TursoEventStore::new(&db_url, None).await.unwrap();
    let state2 = PlatformState::new(None);
    // Verify fresh registry is empty for this tenant.
    {
        let registry = state2.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(
            registry.get_table(&tenant, "Issue").is_none(),
            "fresh registry should be empty"
        );
    }

    // Restore from Turso (this is what build_registry does on boot).
    // Fetch async data outside the lock, then assign synchronously to avoid
    // holding a RwLockWriteGuard across an await point.
    {
        use temper_server::registry::SpecRegistry;
        let mut temp_registry = SpecRegistry::new();
        let restored = restore_registry_from_turso(&mut temp_registry, &turso2)
            .await
            .unwrap();
        assert!(restored > 0, "expected restored specs, got 0");
        *state2.registry.write().unwrap() = temp_registry;
    }

    // Verify specs survived the restart.
    {
        let registry = state2.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
        assert!(registry.get_table(&tenant, "Cycle").is_some());
        assert!(registry.get_table(&tenant, "Comment").is_some());
        assert!(registry.get_table(&tenant, "Label").is_some());
    }

    // Clean up temp DB.
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}
