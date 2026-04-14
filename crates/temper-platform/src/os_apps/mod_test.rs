use super::*;
use std::collections::HashMap;
use std::fs;

use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoSpecVerificationUpdate;
use temper_verify::cascade::VerificationCascade;

#[test]
fn test_pm_specs_parse() {
    let bundle = get_os_app("project-management").expect("PM app not found");
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
    let bundle = get_os_app("project-management").expect("PM app not found");
    let result = parse_csdl(bundle.csdl.as_ref().expect("PM should have CSDL"));
    assert!(
        result.is_ok(),
        "PM CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_pm_spec_entity_names() {
    let bundle = get_os_app("project-management").expect("PM app not found");
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
    let bundle = get_os_app("project-management").expect("PM app not found");
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
    let bundle = get_os_app("agent-orchestration").expect("AO app not found");
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
    let bundle = get_os_app("agent-orchestration").expect("AO app not found");
    let result = parse_csdl(bundle.csdl.as_ref().expect("AO should have CSDL"));
    assert!(
        result.is_ok(),
        "Agent Orchestration CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_agent_orchestration_specs_verify() {
    let bundle = get_os_app("agent-orchestration").expect("AO app not found");
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
        evo.app_guide.is_some(),
        "evolution should have an app guide"
    );
}

#[test]
fn test_intent_discovery_specs_parse() {
    let bundle = get_os_app("intent-discovery").expect("intent-discovery app not found");
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
    let bundle = get_os_app("intent-discovery").expect("intent-discovery app not found");
    let result = parse_csdl(
        bundle
            .csdl
            .as_ref()
            .expect("intent-discovery should have CSDL"),
    );
    assert!(
        result.is_ok(),
        "IntentDiscovery CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_intent_discovery_specs_verify() {
    let bundle = get_os_app("intent-discovery").expect("intent-discovery app not found");
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
    let bundle = get_os_app("project-management");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 5);
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_agent_specs_parse() {
    let bundle = get_os_app("temper-agent").expect("temper-agent app not found");
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
    let bundle = get_os_app("temper-agent").expect("temper-agent app not found");
    let result = parse_csdl(bundle.csdl.as_ref().expect("temper-agent should have CSDL"));
    assert!(
        result.is_ok(),
        "Agent CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_agent_spec_entity_names() {
    let bundle = get_os_app("temper-agent").expect("temper-agent app not found");
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
    let bundle = get_os_app("temper-agent").expect("temper-agent app not found");
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
    let bundle = get_os_app("agent-orchestration");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 3);
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_temper_agent() {
    let bundle = get_os_app("temper-agent");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 8); // TemperAgent + AgentSoul + AgentSkill + AgentMemory + ToolHook + HeartbeatMonitor + CronJob + CronScheduler
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_intent_discovery() {
    let bundle = get_os_app("intent-discovery");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 1);
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_skill_nonexistent() {
    assert!(get_os_app("nonexistent").is_none());
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
async fn test_reinstall_of_skipped_specs_repairs_entity_set_map() {
    let state = PlatformState::new(None);
    let tenant_name = "test-skipped-map-repair";
    let tenant = TenantId::new(tenant_name);

    install_skill(&state, tenant_name, "project-management")
        .await
        .expect("install project-management");

    let bundle = get_os_app("project-management").expect("project-management app not found");
    let mut broken_csdl = bundle.csdl.expect("project-management should have CSDL");
    broken_csdl = broken_csdl.replace(
        r#"        <EntitySet Name="Issues" EntityType="Temper.ProjectManagement.Issue">
          <NavigationPropertyBinding Path="ParentIssue" Target="Issues"/>
          <NavigationPropertyBinding Path="SubIssues" Target="Issues"/>
          <NavigationPropertyBinding Path="Project" Target="Projects"/>
          <NavigationPropertyBinding Path="Cycle" Target="Cycles"/>
          <NavigationPropertyBinding Path="Comments" Target="Comments"/>
        </EntitySet>
"#,
        "",
    );

    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        let parsed = parse_csdl(&broken_csdl).expect("broken CSDL should still parse");
        let specs: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .map(|(entity_type, ioa_source)| (entity_type.as_str(), ioa_source.as_str()))
            .collect();
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant.clone(),
                parsed,
                broken_csdl,
                &specs,
                Vec::new(),
                None,
                false,
            )
            .expect("replace tenant config with a broken entity-set map");

        let verified_at = temper_runtime::scheduler::sim_now().to_rfc3339();
        for (entity_type, _) in &bundle.specs {
            registry.set_verification_status(
                &tenant,
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "Test".to_string(),
                        passed: true,
                        summary: "Preserved verification for skipped reinstall".to_string(),
                        details: None,
                    }],
                    verified_at: verified_at.clone(),
                }),
            );
        }
    }

    {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        assert_eq!(
            registry.resolve_entity_type(&tenant, "Issues").as_deref(),
            None,
            "test setup should remove the Issues entity-set mapping"
        );
        assert!(
            registry.get_table(&tenant, "Issue").is_some(),
            "Issue spec should still exist so reinstall is treated as skipped"
        );
    }

    let reinstall = install_skill(&state, tenant_name, "project-management")
        .await
        .expect("reinstall project-management");

    assert!(reinstall.added.is_empty());
    assert!(reinstall.updated.is_empty());
    assert_eq!(
        reinstall.skipped.len(),
        5,
        "reinstall should still classify all project-management specs as skipped"
    );

    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    assert_eq!(
        registry.resolve_entity_type(&tenant, "Issues").as_deref(),
        Some("Issue"),
        "identical reinstall should repair the entity-set map from the app CSDL"
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
    let issue_row = rows
        .iter()
        .find(|r| r.tenant == "test-ws" && r.entity_type == "Issue")
        .expect("Issue spec should exist");
    assert!(
        issue_row.verified,
        "Issue spec should be durably marked verified after install"
    );
    assert_ne!(
        issue_row.verification_status.to_lowercase(),
        "pending",
        "Issue spec should not remain pending after install"
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
        assert!(
            matches!(
                registry.get_verification_status(&tenant, "Issue"),
                Some(VerificationStatus::Completed(_) | VerificationStatus::Restored(_))
            ),
            "Issue spec should restore with a stable verification status"
        );
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_restore_installed_app_heals_pending_specs_on_restart() {
    let db_path = format!("/tmp/temper-test-heal-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state.server.event_store = Some(std::sync::Arc::new(
        temper_server::event_store::ServerEventStore::Turso(turso),
    ));

    install_skill(&state, "test-heal", "project-management")
        .await
        .expect("install should succeed");

    let turso_ref = state
        .server
        .event_store
        .as_ref()
        .unwrap()
        .platform_turso_store()
        .unwrap();

    for entity_type in ["Issue", "Project", "Cycle", "Comment", "Label"] {
        turso_ref
            .persist_spec_verification(
                "test-heal",
                entity_type,
                TursoSpecVerificationUpdate {
                    status: "pending",
                    verified: false,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
            .unwrap();

        state.registry.write().unwrap().set_verification_status(
            &TenantId::new("test-heal"),
            entity_type,
            VerificationStatus::Pending,
        );
    }

    crate::recovery::restore_installed_apps(&state, turso_ref).await;

    {
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-heal");
        assert!(
            matches!(
                registry.get_verification_status(&tenant, "Issue"),
                Some(VerificationStatus::Completed(_) | VerificationStatus::Restored(_))
            ),
            "Issue spec should be healed out of pending after recovery"
        );
    }

    let rows = turso_ref.load_specs().await.unwrap();
    let issue_row = rows
        .iter()
        .find(|r| r.tenant == "test-heal" && r.entity_type == "Issue")
        .expect("Issue row should exist");
    assert!(
        issue_row.verified,
        "Issue row should be durably re-marked verified during recovery"
    );
    assert_ne!(
        issue_row.verification_status.to_lowercase(),
        "pending",
        "Issue row should not remain pending after recovery"
    );

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

#[test]
fn test_manifest_parses_startup_install_and_wasm_loading_policy() {
    let temp_dir =
        std::env::temp_dir().join(format!("temper-app-manifest-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(
        temp_dir.join("app.toml"),
        r#"name = "core-app"
description = "Core app"
version = "1.0.0"
startup_install = "core"

[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .unwrap();

    let manifest = read_app_manifest(&temp_dir).expect("manifest should parse");
    assert_eq!(manifest.startup_install, StartupInstallMode::Core);
    assert_eq!(manifest.wasm_modules.len(), 1);
    assert_eq!(manifest.wasm_modules[0].name, "echo");
    assert_eq!(
        manifest.wasm_modules[0].criticality,
        WasmModuleCriticality::AppRequired
    );
    assert_eq!(
        manifest.wasm_modules[0].startup_loading,
        WasmStartupLoading::Lazy
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_load_app_bundle_carries_wasm_module_contracts() {
    let temp_dir =
        std::env::temp_dir().join(format!("temper-app-bundle-test-{}", uuid::Uuid::new_v4()));
    let module_dir = temp_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        temp_dir.join("app.toml"),
        r#"name = "bundle-app"
description = "Bundle app"
version = "1.0.0"
startup_install = "manual"

[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .unwrap();
    fs::write(temp_dir.join("APP.md"), "# Bundle App\n\nTest.\n").unwrap();
    fs::copy(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../temper-wasm/tests/fixtures/echo_integration.wasm"),
        module_dir.join("echo.wasm"),
    )
    .unwrap();

    let bundle = load_app_bundle(&temp_dir).expect("bundle should load");
    assert!(bundle.wasm_modules.contains_key("echo"));
    let config = bundle
        .wasm_module_configs
        .get("echo")
        .expect("wasm module config should be present");
    assert_eq!(config.startup_loading, WasmStartupLoading::Lazy);
    assert_eq!(config.criticality, WasmModuleCriticality::AppRequired);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_find_adrs_discovers_markdown_in_sorted_order() {
    let temp_dir = std::env::temp_dir().join(format!("temper-adrs-test-{}", uuid::Uuid::new_v4()));
    let adrs_dir = temp_dir.join("adrs");
    fs::create_dir_all(&adrs_dir).unwrap();
    fs::write(adrs_dir.join("002-second.md"), "# second").unwrap();
    fs::write(adrs_dir.join("001-first.md"), "# first").unwrap();
    fs::write(adrs_dir.join("notes.txt"), "ignore").unwrap();

    let adrs = find_adrs(&temp_dir);
    assert_eq!(adrs.len(), 2);
    assert_eq!(adrs[0].file_name, "001-first.md");
    assert_eq!(adrs[1].file_name, "002-second.md");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_install_app_bootstraps_adrs_into_temper_fs() {
    use std::sync::Arc;
    use temper_server::event_store::ServerEventStore;
    use temper_store_turso::TursoEventStore;

    let app_root = std::env::temp_dir().join(format!("temper-os-apps-{}", uuid::Uuid::new_v4()));
    let app_dir = app_root.join("doc-app");
    fs::create_dir_all(app_dir.join("adrs")).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        "name = \"doc-app\"\ndescription = \"Temporary ADR test app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# Doc App\n\nTemporary ADR test app.\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("adrs/001-initial-design.md"),
        "# ADR-001\n\nBootstrap ADR test.\n",
    )
    .unwrap();
    add_os_apps_dir(app_root.clone());

    let db_path = format!("/tmp/temper-adr-test-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state.server.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

    // Re-add the temp dir before each install — the concurrent
    // `test_reload_picks_up_disk_changes` test calls `reload_skills()`
    // which replaces the global catalog, potentially wiping our entry.
    add_os_apps_dir(app_root.clone());
    install_os_app(&state, "test-adr-app", "temper-fs")
        .await
        .expect("install temper-fs");
    add_os_apps_dir(app_root.clone());
    let result = install_os_app(&state, "test-adr-app", "doc-app")
        .await
        .expect("install doc-app");

    assert_eq!(
        result.adrs_bootstrapped,
        vec!["/apps/doc-app/adrs/001-initial-design.md".to_string()]
    );

    let tenant = TenantId::new("test-adr-app");
    let mut found = false;
    for file_id in state.server.list_entity_ids(&tenant, "File") {
        let resp = state
            .server
            .get_tenant_entity_state(&tenant, "File", &file_id)
            .await
            .unwrap();
        let path = resp
            .state
            .fields
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if path == "/apps/doc-app/adrs/001-initial-design.md" {
            found = true;
            assert_eq!(resp.state.status, "Ready");
            assert_eq!(resp.state.booleans.get("has_content"), Some(&true));
            assert!(
                resp.state
                    .fields
                    .get("content_hash")
                    .and_then(|value| value.as_str())
                    .is_some()
            );
        }
    }
    assert!(found, "expected ADR file entity to exist in TemperFS");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path}-wal"));
    let _ = fs::remove_file(format!("{db_path}-shm"));
}
