use super::*;
use std::collections::HashMap;

use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::VerificationCascade;

#[test]
fn test_pm_specs_parse() {
    let bundle = get_skill("project-management").expect("PM skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
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
    let bundle = get_skill("project-management").expect("PM skill not found");
    let result = parse_csdl(&bundle.csdl);
    assert!(
        result.is_ok(),
        "PM CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_pm_spec_entity_names() {
    let bundle = get_skill("project-management").expect("PM skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let a = automaton::parse_automaton(ioa_source).unwrap();
        assert_eq!(
            &a.automaton.name, entity_type,
            "PM spec name mismatch: expected {entity_type}, got {}",
            a.automaton.name
        );
    }
}

#[test]
fn test_pm_specs_verify() {
    let bundle = get_skill("project-management").expect("PM skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
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
    let bundle = get_skill("agent-orchestration").expect("AO skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
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
    let bundle = get_skill("agent-orchestration").expect("AO skill not found");
    let result = parse_csdl(&bundle.csdl);
    assert!(
        result.is_ok(),
        "Agent Orchestration CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_agent_orchestration_specs_verify() {
    let bundle = get_skill("agent-orchestration").expect("AO skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
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
fn test_list_skills_returns_catalog() {
    let apps = list_skills();
    // Should find the built-in spec-bearing skills.
    let names: Vec<&str> = apps.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"project-management"),
        "missing project-management: {names:?}"
    );
    assert!(names.contains(&"temper-fs"), "missing temper-fs: {names:?}");
    assert!(
        names.contains(&"agent-orchestration"),
        "missing agent-orchestration: {names:?}"
    );
    assert!(
        names.contains(&"temper-agent"),
        "missing temper-agent: {names:?}"
    );
    assert!(names.contains(&"evolution"), "missing evolution: {names:?}");
    assert!(
        names.contains(&"intent-discovery"),
        "missing intent-discovery: {names:?}"
    );

    let pm = apps
        .iter()
        .find(|e| e.name == "project-management")
        .unwrap();
    assert_eq!(
        pm.entity_types.len(),
        5,
        "PM entity types: {:?}",
        pm.entity_types
    );
    let evo = apps.iter().find(|e| e.name == "evolution").unwrap();
    assert_eq!(
        evo.entity_types.len(),
        2,
        "Evo entity types: {:?}",
        evo.entity_types
    );
    assert!(
        evo.skill_guide.is_some(),
        "evolution should have a skill guide"
    );
}

#[test]
fn test_intent_discovery_specs_parse() {
    let bundle = get_skill("intent-discovery").expect("intent-discovery skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let result = automaton::parse_automaton(ioa_source);
        assert!(
            result.is_ok(),
            "IntentDiscovery spec {} failed to parse: {:?}",
            entity_type,
            result.err()
        );
    }
}

#[test]
fn test_intent_discovery_csdl_parses() {
    let bundle = get_skill("intent-discovery").expect("intent-discovery skill not found");
    let result = parse_csdl(&bundle.csdl);
    assert!(
        result.is_ok(),
        "IntentDiscovery CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_intent_discovery_specs_verify() {
    let bundle = get_skill("intent-discovery").expect("intent-discovery skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(40);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "IntentDiscovery spec {} failed verification",
            entity_type
        );
    }
}

#[test]
fn test_get_skill_project_management() {
    let bundle = get_skill("project-management");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 5);
    assert!(!bundle.csdl.is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_agent_specs_parse() {
    let bundle = get_skill("temper-agent").expect("temper-agent skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let result = automaton::parse_automaton(ioa_source);
        assert!(
            result.is_ok(),
            "Agent spec {} failed to parse: {:?}",
            entity_type,
            result.err()
        );
    }
}

#[test]
fn test_agent_csdl_parses() {
    let bundle = get_skill("temper-agent").expect("temper-agent skill not found");
    let result = parse_csdl(&bundle.csdl);
    assert!(
        result.is_ok(),
        "Agent CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_agent_spec_entity_names() {
    let bundle = get_skill("temper-agent").expect("temper-agent skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let a = automaton::parse_automaton(ioa_source).unwrap();
        assert_eq!(
            &a.automaton.name, entity_type,
            "Agent spec name mismatch: expected {entity_type}, got {}",
            a.automaton.name
        );
    }
}

#[test]
fn test_agent_specs_verify() {
    let bundle = get_skill("temper-agent").expect("temper-agent skill not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "Agent spec {} failed verification",
            entity_type
        );
    }
}

#[test]
fn test_get_skill_agent_orchestration() {
    let bundle = get_skill("agent-orchestration");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 3);
    assert!(!bundle.csdl.is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_temper_agent() {
    let bundle = get_skill("temper-agent");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 8); // TemperAgent + AgentSoul + AgentSkill + AgentMemory + ToolHook + HeartbeatMonitor + CronJob + CronScheduler
    assert!(!bundle.csdl.is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_intent_discovery() {
    let bundle = get_skill("intent-discovery");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 1);
    assert!(!bundle.csdl.is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_nonexistent() {
    assert!(get_skill("nonexistent").is_none());
}

#[tokio::test]
async fn test_install_skill_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_skill(&state, "test-pm", "project-management").await;
    assert!(result.is_ok());
    let result = result.unwrap();
    // Fresh tenant — all 5 specs should be new.
    assert_eq!(
        result.added.len(),
        5,
        "expected 5 added: {:?}",
        result.added
    );
    assert!(result.updated.is_empty());
    assert!(result.skipped.is_empty());
    assert!(result.added.contains(&"Issue".to_string()));
    assert!(result.added.contains(&"Project".to_string()));
    assert!(result.added.contains(&"Cycle".to_string()));
    assert!(result.added.contains(&"Comment".to_string()));
    assert!(result.added.contains(&"Label".to_string()));

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
async fn test_install_skill_agent_orchestration_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_skill(&state, "test-ao", "agent-orchestration").await;
    assert!(result.is_ok());
    let result = result.unwrap();
    assert_eq!(
        result.added.len(),
        3,
        "expected 3 added: {:?}",
        result.added
    );
    assert!(result.updated.is_empty());
    assert!(result.skipped.is_empty());
    assert!(result.added.contains(&"HeartbeatRun".to_string()));
    assert!(result.added.contains(&"Organization".to_string()));
    assert!(result.added.contains(&"BudgetLedger".to_string()));

    let registry = state.registry.read().unwrap();
    let tenant = TenantId::new("test-ao");
    assert!(registry.get_table(&tenant, "HeartbeatRun").is_some());
    assert!(registry.get_table(&tenant, "Organization").is_some());
    assert!(registry.get_table(&tenant, "BudgetLedger").is_some());
}

#[tokio::test]
async fn test_install_temper_agent_auto_installs_temper_fs() {
    let state = PlatformState::new(None);
    install_os_app(&state, "test-agent", "temper-agent")
        .await
        .expect("install temper-agent");
    let registry = state.registry.read().unwrap();
    let tenant = TenantId::new("test-agent");
    for entity in [
        "TemperAgent",
        "Workspace",
        "File",
        "Directory",
        "FileVersion",
    ] {
        assert!(
            registry.get_table(&tenant, entity).is_some(),
            "missing {entity}"
        );
    }
}

#[tokio::test]
async fn test_install_skill_nonexistent_returns_error() {
    let state = PlatformState::new(None);
    let result = install_skill(&state, "test", "nonexistent").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found in catalog"));
}

#[tokio::test]
async fn test_install_multiple_skills_merges_and_is_idempotent() {
    let state = PlatformState::new(None);
    let tenant = TenantId::new("test-merge");

    install_skill(&state, "test-merge", "project-management")
        .await
        .expect("install project-management");

    install_skill(&state, "test-merge", "agent-orchestration")
        .await
        .expect("install agent-orchestration");

    {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        for entity_type in [
            "Issue",
            "Project",
            "Cycle",
            "Comment",
            "Label",
            "HeartbeatRun",
            "Organization",
            "BudgetLedger",
        ] {
            assert!(
                registry.get_table(&tenant, entity_type).is_some(),
                "{entity_type} should remain available after multi-app install"
            );
        }

        // Existing tenant mappings should still resolve after app merge.
        assert_eq!(
            registry.resolve_entity_type(&tenant, "Issues").as_deref(),
            Some("Issue")
        );
        assert_eq!(
            registry
                .resolve_entity_type(&tenant, "HeartbeatRuns")
                .as_deref(),
            Some("HeartbeatRun")
        );
    }

    let reinstall = install_skill(&state, "test-merge", "project-management")
        .await
        .expect("reinstall project-management");

    // Reinstall of identical specs should skip all 5.
    assert!(
        reinstall.added.is_empty(),
        "no new entities expected on reinstall"
    );
    assert!(
        reinstall.updated.is_empty(),
        "no updates expected on reinstall of identical specs"
    );
    assert_eq!(
        reinstall.skipped.len(),
        5,
        "all 5 PM specs should be skipped on identical reinstall"
    );

    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let mut entity_types = registry
        .entity_types(&tenant)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    entity_types.sort();

    assert_eq!(
        entity_types,
        vec![
            "BudgetLedger".to_string(),
            "Comment".to_string(),
            "Cycle".to_string(),
            "HeartbeatRun".to_string(),
            "Issue".to_string(),
            "Label".to_string(),
            "Organization".to_string(),
            "Project".to_string(),
        ]
    );
}

#[tokio::test]
async fn test_install_skill_activates_tenant_cedar_policies() {
    let state = PlatformState::new(None);

    install_skill(&state, "test-authz", "project-management")
        .await
        .expect("install project-management");

    let admin_ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "admin-1".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
    ]);
    let mut issue_attrs = HashMap::new();
    issue_attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let admin_decision = state.server.authz.authorize_for_tenant(
        "test-authz",
        &admin_ctx,
        "MoveToTodo",
        "Issue",
        &issue_attrs,
    );
    assert!(
        admin_decision.is_allowed(),
        "expected admin Issue.MoveToTodo to be allowed after skill install: {admin_decision:?}"
    );

    install_skill(&state, "test-authz", "temper-agent")
        .await
        .expect("install temper-agent");

    let mut agent_attrs = HashMap::new();
    agent_attrs.insert("id".to_string(), serde_json::json!("agent-1"));

    let configure_decision = state.server.authz.authorize_for_tenant(
        "test-authz",
        &admin_ctx,
        "Configure",
        "TemperAgent",
        &agent_attrs,
    );
    assert!(
        configure_decision.is_allowed(),
        "expected admin TemperAgent.Configure to be allowed after skill install: {configure_decision:?}"
    );
}

/// Proves the full install → persist → reboot → restore cycle.
///
/// 1. Install OS app with a real Turso-backed SQLite DB.
/// 2. Verify specs land in both registry and Turso.
/// 3. Build a fresh PlatformState (simulating restart) with the same DB.
/// 4. Restore registry from Turso.
/// 5. Verify specs survived the "restart".
#[tokio::test]
async fn test_skill_install_survives_restart() {
    use std::sync::Arc;
    use temper_server::event_store::ServerEventStore;
    use temper_server::registry_bootstrap::restore_registry_from_turso;
    use temper_store_turso::TursoEventStore;

    let db_path = format!("/tmp/temper-test-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state.server.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

    let result = install_skill(&state, "test-ws", "project-management").await;
    assert!(result.is_ok(), "install failed: {:?}", result.err());
    let result = result.unwrap();
    assert_eq!(result.added.len(), 5);

    {
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
    }

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

    let installed = turso_ref.list_all_installed_apps().await.unwrap();
    assert!(
        installed.contains(&("test-ws".to_string(), "project-management".to_string())),
        "installed app record not found"
    );

    let turso2 = TursoEventStore::new(&db_url, None).await.unwrap();
    let state2 = PlatformState::new(None);
    {
        let registry = state2.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(
            registry.get_table(&tenant, "Issue").is_none(),
            "fresh registry should be empty"
        );
    }

    {
        use temper_server::registry::SpecRegistry;
        let mut temp_registry = SpecRegistry::new();
        let restored = restore_registry_from_turso(&mut temp_registry, &turso2)
            .await
            .unwrap();
        assert!(restored > 0, "expected restored specs, got 0");
        *state2.registry.write().unwrap() = temp_registry;
    }

    {
        let registry = state2.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
        assert!(registry.get_table(&tenant, "Cycle").is_some());
        assert!(registry.get_table(&tenant, "Comment").is_some());
        assert!(registry.get_table(&tenant, "Label").is_some());
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[test]
fn test_reload_picks_up_disk_changes() {
    reload_skills();
    let skills = list_skills();
    assert!(
        !skills.is_empty(),
        "catalog should not be empty after reload"
    );
}
