use super::agent_bootstrap::{
    AgentSoulRefreshDecision, bootstrapped_agent_soul_entity_id, decide_agent_soul_refresh,
};
use super::*;
use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoSpecVerificationUpdate;
use temper_verify::cascade::VerificationCascade;

fn test_admin_security_context(principal_id: &str) -> SecurityContext {
    SecurityContext {
        principal: temper_authz::Principal {
            id: principal_id.to_string(),
            kind: temper_authz::PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "test-admin-context".to_string(),
    }
}

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
fn app_manifest_rejects_duplicate_module_names() {
    let module = WasmModuleManifest {
        name: "worker".into(),
        target: None,
        criticality: WasmModuleCriticality::Optional,
        startup_loading: WasmStartupLoading::Lazy,
        provenance: None,
        import_class: None,
        data: None,
        data_binding: None,
    };
    let manifest = AppManifest {
        name: "demo".into(),
        description: String::new(),
        version: "1.0.0".into(),
        mode: AppDeploymentMode::Operator,
        startup_install: StartupInstallMode::Manual,
        dependencies: Vec::new(),
        wasm_modules: vec![module.clone(), module],
    };
    assert_eq!(
        manifest.validate().unwrap_err(),
        "duplicate WASM module 'worker'"
    );
}

#[test]
fn app_manifest_rejects_unbound_data_grant() {
    let manifest = AppManifest {
        name: "demo".into(),
        description: String::new(),
        version: "1.0.0".into(),
        mode: AppDeploymentMode::Operator,
        startup_install: StartupInstallMode::Manual,
        dependencies: Vec::new(),
        wasm_modules: vec![WasmModuleManifest {
            name: "worker".into(),
            target: None,
            criticality: WasmModuleCriticality::Optional,
            startup_loading: WasmStartupLoading::Lazy,
            provenance: None,
            import_class: None,
            data: Some(temper_wasm_sdk::data::ModuleDataGrant::default()),
            data_binding: None,
        }],
    };
    assert!(
        manifest
            .validate()
            .unwrap_err()
            .contains("requires data_binding")
    );
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
fn test_list_os_apps_returns_catalog() {
    let apps = list_os_apps();
    // Should find the built-in spec-bearing apps.
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
    assert!(
        names.contains(&"directed-evolution"),
        "missing directed-evolution: {names:?}"
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
    let directed = apps
        .iter()
        .find(|e| e.name == "directed-evolution")
        .unwrap();
    assert_eq!(
        directed.entity_types.len(),
        26,
        "Directed Evolution entity types: {:?}",
        directed.entity_types
    );
    assert!(
        directed.app_guide.is_some(),
        "directed-evolution should have an app guide"
    );
}

#[test]
fn test_resolve_os_app_install_order_dedupes_shared_dependencies() {
    let dependencies = HashMap::from([
        ("base", Vec::<String>::new()),
        ("left", vec!["base".to_string()]),
        ("right", vec!["base".to_string()]),
        ("top", vec!["left".to_string(), "right".to_string()]),
    ]);
    let order = reconcile::resolve_os_app_install_order_with_dependencies(
        &["top".to_string(), "right".to_string()],
        |app_name| dependencies.get(app_name).cloned().unwrap_or_default(),
    )
    .unwrap();

    assert_eq!(order, vec!["base", "left", "right", "top"]);
}

#[test]
fn test_manifest_dependencies_accept_pinned_genesis_refs_for_local_install_order() {
    assert_eq!(
        local_os_app_dependency_name("temperpaw/paw-fs@65f3ee9659500d11a54c22b9e5519d52dd0db1d4")
            .as_deref(),
        Some("paw-fs")
    );
    assert_eq!(
        local_os_app_dependency_name("katagami-commons").as_deref(),
        Some("katagami-commons")
    );
    assert_eq!(local_os_app_dependency_name("  "), None);
}

#[test]
fn test_os_app_document_bootstrap_does_not_charge_workspace_file_count() {
    let source = include_str!("mod.rs");
    assert!(
        !source.contains("action: \"IncrementFileCount\""),
        "OS app document bootstrap must not charge Workspace for each file materialized"
    );
}

#[test]
fn test_reconcile_plan_for_wasm_only_digest_skips_unrelated_phases() {
    let current = OsAppBundleDigest {
        app_name: "paw-agent".to_string(),
        app_version: "0.1.0".to_string(),
        bundle_digest: "sha256:bundle-current".to_string(),
        spec_digest: "sha256:spec-current".to_string(),
        policy_digest: "sha256:policy-current".to_string(),
        wasm_digest: "sha256:wasm-current".to_string(),
        content_digest: "sha256:content-current".to_string(),
        seed_digest: "sha256:seed-current".to_string(),
    };
    let installed = InstalledAppRecord {
        tenant: "default".to_string(),
        app_name: current.app_name.clone(),
        source_kind: "local".to_string(),
        app_ref: String::new(),
        version_hash: String::new(),
        pinned_version_hash: String::new(),
        current_version_hash: String::new(),
        follow_policy: "pinned".to_string(),
        closure_id: String::new(),
        registry_url: String::new(),
        registry_tenant: String::new(),
        dependency_lock_digest: String::new(),
        app_version: current.app_version.clone(),
        bundle_digest: "sha256:bundle-old".to_string(),
        spec_digest: current.spec_digest.clone(),
        policy_digest: current.policy_digest.clone(),
        wasm_digest: "sha256:wasm-old".to_string(),
        content_digest: current.content_digest.clone(),
        seed_digest: current.seed_digest.clone(),
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    };

    let plan =
        reconcile::plan_reconcile_from_installed_record(&installed, &current, true, true, true);

    assert_eq!(
        plan,
        OsAppInstallPlan {
            specs: false,
            policies: false,
            wasm: true,
            content: false,
            seed: false,
        }
    );
}

#[tokio::test]
async fn test_reconcile_os_app_skips_unchanged_bundle_digest() {
    let db_path = format!("/tmp/temper-test-digest-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, "test-digest", "project-management")
        .await
        .expect("initial install should succeed");

    let result = reconcile_os_app(&state, "test-digest", "project-management")
        .await
        .expect("unchanged reconcile should succeed");

    assert!(
        matches!(result, OsAppReconcileResult::Skipped { .. }),
        "unchanged app should skip hot reinstall, got {result:?}"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_repairs_missing_active_policies_for_unchanged_bundle() {
    let db_path = format!(
        "/tmp/temper-test-policy-reconcile-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let tenant = "test-policy-reconcile";

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, tenant, "project-management")
        .await
        .expect("initial install should succeed");

    {
        let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.remove(tenant);
    }
    state
        .server
        .authz
        .reload_tenant_policies(tenant, "")
        .expect("empty tenant policy should load");

    let admin_ctx = test_admin_security_context("admin-1");
    let mut issue_attrs = HashMap::new();
    issue_attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let denied_before_reconcile = state.server.authz.authorize_for_tenant(
        tenant,
        &admin_ctx,
        "MoveToTodo",
        "Issue",
        &issue_attrs,
    );
    assert!(
        !denied_before_reconcile.is_allowed(),
        "test setup should remove active app policies before reconcile"
    );

    let result = reconcile_os_app(&state, tenant, "project-management")
        .await
        .expect("unchanged reconcile should repair missing active policies");

    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("missing active policies should force a policies-only reconcile");
    };
    assert!(install.added.is_empty());
    assert!(install.updated.is_empty());
    assert!(install.skipped.is_empty());

    let allowed_after_reconcile = state.server.authz.authorize_for_tenant(
        tenant,
        &admin_ctx,
        "MoveToTodo",
        "Issue",
        &issue_attrs,
    );
    assert!(
        allowed_after_reconcile.is_allowed(),
        "reconcile should reload the app Cedar policies when active memory lost them: {allowed_after_reconcile:?}"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_repairs_missing_authz_engine_policies_despite_text_cache() {
    let db_path = format!(
        "/tmp/temper-test-policy-cache-reconcile-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let tenant = "test-policy-cache-reconcile";

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, tenant, "project-management")
        .await
        .expect("initial install should succeed");

    let cached_policy_text = {
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        policies
            .get(tenant)
            .cloned()
            .expect("initial install should cache app policies")
    };
    state
        .server
        .authz
        .reload_tenant_policies(tenant, "")
        .expect("empty tenant policy should load");
    {
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        assert_eq!(
            policies.get(tenant).map(String::as_str),
            Some(cached_policy_text.as_str()),
            "test setup should leave cached policy text intact"
        );
    }

    let admin_ctx = test_admin_security_context("admin-1");
    let mut issue_attrs = HashMap::new();
    issue_attrs.insert("id".to_string(), serde_json::json!("issue-1"));

    let denied_before_reconcile = state.server.authz.authorize_for_tenant(
        tenant,
        &admin_ctx,
        "MoveToTodo",
        "Issue",
        &issue_attrs,
    );
    assert!(
        !denied_before_reconcile.is_allowed(),
        "test setup should remove only the active Cedar authorizer policies"
    );

    let result = reconcile_os_app(&state, tenant, "project-management")
        .await
        .expect("unchanged reconcile should repair missing active authz policies");

    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("missing authz policies should force a policies-only reconcile");
    };
    assert!(install.added.is_empty());
    assert!(install.updated.is_empty());
    assert!(install.skipped.is_empty());

    let active_policy_text = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .expect("reconcile should reload active app policies");
    assert_eq!(
        active_policy_text.trim(),
        cached_policy_text.trim(),
        "repair should reload cached app policies without duplicating them"
    );

    let allowed_after_reconcile = state.server.authz.authorize_for_tenant(
        tenant,
        &admin_ctx,
        "MoveToTodo",
        "Issue",
        &issue_attrs,
    );
    assert!(
        allowed_after_reconcile.is_allowed(),
        "reconcile should reload cached app policies into the active Cedar authorizer: {allowed_after_reconcile:?}"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_runtime_recovery_requires_active_policies_for_ready_outcome() {
    let db_path = format!(
        "/tmp/temper-test-runtime-policy-readiness-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let tenant = "test-runtime-policy-readiness";

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, tenant, "project-management")
        .await
        .expect("initial install should succeed");

    let turso_ref = state.server.platform_turso_store().unwrap();
    let cached_policy_text = {
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        policies
            .get(tenant)
            .cloned()
            .expect("initial install should cache app policies")
    };
    state
        .server
        .authz
        .reload_tenant_policies(tenant, "")
        .expect("empty tenant policy should load");

    let outcome = crate::recovery::recover_installed_app_runtime_state(
        &state,
        &turso_ref,
        tenant,
        "project-management",
    )
    .await;

    assert_eq!(
        outcome,
        crate::recovery::InstalledAppRuntimeRecoveryOutcome::NeedsReconcile,
        "runtime recovery must not mark an unchanged app ready when active Cedar policies are missing"
    );
    assert_eq!(
        state
            .server
            .tenant_policies
            .read()
            .unwrap()
            .get(tenant)
            .map(String::as_str),
        Some(cached_policy_text.as_str()),
        "test setup should preserve cached policy text while active authz is empty"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_install_plan_without_spec_phase_does_not_reclassify_specs() {
    let state = PlatformState::new(None);
    let tenant = "test-install-plan-skip-specs";

    install_os_app(&state, tenant, "project-management")
        .await
        .expect("initial install should succeed");

    let result = install_os_app_with_plan(
        &state,
        tenant,
        "project-management",
        OsAppInstallPlan {
            specs: false,
            policies: false,
            wasm: false,
            content: false,
            seed: false,
        },
    )
    .await
    .expect("planned no-op install should succeed");

    assert!(result.added.is_empty());
    assert!(result.updated.is_empty());
    assert!(
        result.skipped.is_empty(),
        "spec classification should not run when the spec phase is disabled"
    );
    assert!(result.wasm_modules.is_empty());
    assert!(result.agents.is_empty());
    assert!(result.skills.is_empty());
    assert!(result.seed_instances.is_empty());
}

#[tokio::test]
async fn test_reconcile_os_app_delta_content_change_skips_specs() {
    let db_path = format!("/tmp/temper-test-delta-content-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let tenant = "test-delta-content";

    install_os_app(&state, tenant, "project-management")
        .await
        .expect("initial install should succeed");

    let current = os_app_bundle_digest("project-management").expect("project-management digest");
    let ps = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .expect("platform store");
    ps.record_installed_app_metadata(&InstalledAppRecord {
        tenant: tenant.to_string(),
        app_name: current.app_name.clone(),
        source_kind: "local".to_string(),
        app_ref: String::new(),
        version_hash: String::new(),
        pinned_version_hash: String::new(),
        current_version_hash: String::new(),
        follow_policy: "pinned".to_string(),
        closure_id: String::new(),
        registry_url: String::new(),
        registry_tenant: String::new(),
        dependency_lock_digest: String::new(),
        app_version: current.app_version.clone(),
        bundle_digest: "sha256:previous-bundle".to_string(),
        spec_digest: current.spec_digest.clone(),
        policy_digest: current.policy_digest.clone(),
        wasm_digest: current.wasm_digest.clone(),
        content_digest: "sha256:previous-content".to_string(),
        seed_digest: current.seed_digest.clone(),
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    })
    .await
    .expect("overwrite installed-app metadata");

    let result = reconcile_os_app(&state, tenant, "project-management")
        .await
        .expect("delta reconcile should succeed");

    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("content digest change should run delta install");
    };
    assert!(install.added.is_empty());
    assert!(install.updated.is_empty());
    assert!(
        install.skipped.is_empty(),
        "content-only reconcile should not reclassify or bootstrap specs"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_repairs_spec_content_drift_despite_matching_digest() {
    let db_path = format!("/tmp/temper-test-spec-drift-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let tenant_name = "test-spec-drift";
    let tenant = TenantId::new(tenant_name);

    install_os_app(&state, tenant_name, "project-management")
        .await
        .expect("initial install should succeed");

    let bundle = get_os_app("project-management").expect("project-management app not found");
    let csdl = bundle
        .csdl
        .clone()
        .expect("project-management should have CSDL");
    let mut drifted_specs = bundle.specs.clone();
    let drifted_entity = drifted_specs
        .first()
        .map(|(entity_type, _)| entity_type.clone())
        .expect("project-management should have specs");
    drifted_specs[0].1.push_str("\n# test-only spec drift\n");

    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        let parsed = parse_csdl(&csdl).expect("CSDL should parse");
        let specs: Vec<(&str, &str)> = drifted_specs
            .iter()
            .map(|(entity_type, ioa_source)| (entity_type.as_str(), ioa_source.as_str()))
            .collect();
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant.clone(),
                parsed,
                csdl,
                &specs,
                Vec::new(),
                bundle.cross_invariants_toml.clone(),
                true,
            )
            .expect("replace tenant config with drifted spec source");

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
                        summary: "Spec drift should still force app reconcile".to_string(),
                        details: None,
                    }],
                    verified_at: verified_at.clone(),
                }),
            );
        }
    }

    let result = reconcile_os_app(&state, tenant_name, "project-management")
        .await
        .expect("reconcile should repair spec content drift");

    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("matching digest with drifted runtime spec content must not skip reconcile");
    };
    assert!(
        install.updated.contains(&drifted_entity),
        "drifted spec should be reclassified as updated: {install:?}"
    );
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let repaired = registry
        .get_spec(&tenant, &drifted_entity)
        .expect("drifted spec should still be registered");
    let expected = bundle
        .specs
        .iter()
        .find(|(entity_type, _)| entity_type == &drifted_entity)
        .map(|(_, ioa_source)| ioa_source.as_str())
        .expect("bundle should still contain drifted entity");
    assert_eq!(repaired.ioa_source, expected);

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_repairs_entity_set_map_from_matching_digest() {
    let db_path = format!("/tmp/temper-test-reconcile-map-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let tenant_name = "test-reconcile-map";
    let tenant = TenantId::new(tenant_name);

    install_os_app(&state, tenant_name, "project-management")
        .await
        .expect("initial install should succeed");

    let bundle = get_os_app("project-management").expect("project-management app not found");
    let mut broken_csdl = bundle
        .csdl
        .clone()
        .expect("project-management should have CSDL");
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
                        summary: "Preserved verification for skipped reconcile".to_string(),
                        details: None,
                    }],
                    verified_at: verified_at.clone(),
                }),
            );
        }
    }

    let result = reconcile_os_app(&state, tenant_name, "project-management")
        .await
        .expect("reconcile should heal missing entity-set map without reinstalling content");

    assert!(
        matches!(result, OsAppReconcileResult::Skipped { .. }),
        "matching digest should repair OData entity-set mappings without reinstall, got {result:?}"
    );
    assert_eq!(
        state
            .registry
            .read()
            .unwrap()
            .resolve_entity_type(&tenant, "Issues")
            .as_deref(),
        Some("Issue")
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
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

mod directed_evolution;
#[test]
fn test_get_app_project_management() {
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
fn test_get_app_agent_orchestration() {
    let bundle = get_os_app("agent-orchestration");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 3);
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_app_temper_agent() {
    let bundle = get_os_app("temper-agent");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 8); // TemperAgent + AgentSoul + AgentSkill + AgentMemory + ToolHook + HeartbeatMonitor + CronJob + CronScheduler
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_app_intent_discovery() {
    let bundle = get_os_app("intent-discovery");
    assert!(bundle.is_some());
    let bundle = bundle.unwrap();
    assert_eq!(bundle.specs.len(), 1);
    assert!(bundle.csdl.is_some());
    assert!(!bundle.csdl.as_ref().unwrap().is_empty());
    assert!(!bundle.cedar_policies.is_empty());
}

#[test]
fn test_get_app_nonexistent() {
    assert!(get_os_app("nonexistent").is_none());
}

#[test]
fn test_find_wasm_modules_discovers_packaged_root_wasm() {
    let root =
        std::env::temp_dir().join(format!("temper-os-app-wasm-test-{}", uuid::Uuid::new_v4()));
    let module_dir = root.join("wasm").join("demo_module");
    fs::create_dir_all(&module_dir).expect("create module dir");
    fs::write(module_dir.join("demo_module.wasm"), b"\0asm-packaged").expect("write wasm");

    let mut configs = BTreeMap::new();
    configs.insert(
        "demo_module".to_string(),
        WasmModuleManifest {
            name: "demo_module".to_string(),
            target: None,
            criticality: WasmModuleCriticality::default(),
            startup_loading: WasmStartupLoading::default(),
            provenance: None,
            import_class: None,
            data: None,
            data_binding: None,
        },
    );

    let modules = find_wasm_modules(&root, &configs);
    fs::remove_dir_all(&root).expect("remove temp app");

    assert_eq!(
        modules.get("demo_module").map(Vec::as_slice),
        Some(&b"\0asm-packaged"[..])
    );
}

#[tokio::test]
async fn test_install_os_app_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-pm", "project-management").await;
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
async fn test_install_os_app_agent_orchestration_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-ao", "agent-orchestration").await;
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

#[test]
fn test_bootstrapped_agent_soul_ids_are_stable_and_slugged() {
    assert_eq!(
        bootstrapped_agent_soul_entity_id("Paw"),
        "sl-bootstrap-agent-soul-paw"
    );
    assert_eq!(
        bootstrapped_agent_soul_entity_id("Reliability Lead"),
        "sl-bootstrap-agent-soul-reliability-lead"
    );
}

#[tokio::test]
async fn test_install_os_app_nonexistent_returns_error() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test", "nonexistent").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found in catalog"));
}

#[tokio::test]
async fn test_install_multiple_apps_merges_and_is_idempotent() {
    let state = PlatformState::new(None);
    let tenant = TenantId::new("test-merge");

    install_os_app(&state, "test-merge", "project-management")
        .await
        .expect("install project-management");

    install_os_app(&state, "test-merge", "agent-orchestration")
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

    let reinstall = install_os_app(&state, "test-merge", "project-management")
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

    install_os_app(&state, tenant_name, "project-management")
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

    let reinstall = install_os_app(&state, tenant_name, "project-management")
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
async fn test_install_os_app_activates_tenant_cedar_policies() {
    let state = PlatformState::new(None);

    install_os_app(&state, "test-authz", "project-management")
        .await
        .expect("install project-management");

    let admin_ctx = test_admin_security_context("admin-1");
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
        "expected admin Issue.MoveToTodo to be allowed after app install: {admin_decision:?}"
    );

    install_os_app(&state, "test-authz", "temper-agent")
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
        "expected admin TemperAgent.Configure to be allowed after app install: {configure_decision:?}"
    );
}

#[tokio::test]
async fn test_install_os_app_persists_granular_policy_rows() {
    use temper_store_turso::TursoEventStore;

    let db_path = format!("/tmp/temper-policy-rows-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, "test-policy-rows", "project-management")
        .await
        .expect("install project-management");

    let policy_store = state.server.policy_store().expect("policy store");
    let rows = policy_store
        .load_policies_for_tenant("test-policy-rows")
        .await
        .expect("load granular policies");
    assert!(
        rows.iter().any(|row| {
            row.policy_id == "project-management-issue"
                && row.enabled
                && row.cedar_text.contains("resource is Issue")
        }),
        "project-management install should persist policies/issue.cedar as a granular row: {rows:?}"
    );

    let turso_ref = state.server.platform_turso_store().unwrap();
    let legacy_rows = turso_ref.load_tenant_policies().await.unwrap();
    assert!(
        legacy_rows
            .iter()
            .any(|(tenant, text)| tenant == "test-policy-rows" && text.contains("Issue")),
        "legacy aggregate policy should still be persisted for compatibility"
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
async fn test_app_install_survives_restart() {
    use temper_server::registry_bootstrap::restore_registry_from_turso;
    use temper_store_turso::TursoEventStore;

    let db_path = format!("/tmp/temper-test-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-adr-test-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();

    let result = install_os_app(&state, "test-ws", "project-management").await;
    assert!(result.is_ok(), "install failed: {:?}", result.err());
    let result = result.unwrap();
    assert_eq!(result.added.len(), 5);

    {
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-ws");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
    }

    let turso_ref = state.server.platform_turso_store().unwrap();
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
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, "test-heal", "project-management")
        .await
        .expect("install should succeed");

    let turso_ref = state.server.platform_turso_store().unwrap();

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

    crate::recovery::restore_installed_apps(&state, &turso_ref).await;

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

#[tokio::test]
async fn test_runtime_recovery_heals_matching_digest_without_hot_reinstall() {
    let db_path = format!("/tmp/temper-test-runtime-heal-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));

    install_os_app(&state, "test-runtime-heal", "project-management")
        .await
        .expect("install should succeed");

    let turso_ref = state.server.platform_turso_store().unwrap();

    for entity_type in ["Issue", "Project", "Cycle", "Comment", "Label"] {
        turso_ref
            .persist_spec_verification(
                "test-runtime-heal",
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
            &TenantId::new("test-runtime-heal"),
            entity_type,
            VerificationStatus::Pending,
        );
    }

    let outcome = crate::recovery::recover_installed_app_runtime_state(
        &state,
        &turso_ref,
        "test-runtime-heal",
        "project-management",
    )
    .await;

    assert_eq!(
        outcome,
        crate::recovery::InstalledAppRuntimeRecoveryOutcome::Healed,
        "matching digest recovery should heal runtime spec readiness without reinstalling content"
    );

    let result = reconcile_os_app(&state, "test-runtime-heal", "project-management")
        .await
        .expect("unchanged reconcile should succeed after runtime recovery");
    assert!(
        matches!(result, OsAppReconcileResult::Skipped { .. }),
        "runtime-healed app should skip hot reinstall, got {result:?}"
    );

    let rows = turso_ref.load_specs().await.unwrap();
    let issue_row = rows
        .iter()
        .find(|r| r.tenant == "test-runtime-heal" && r.entity_type == "Issue")
        .expect("Issue row should exist");
    assert!(
        issue_row.verified,
        "runtime heal should persist verified=true"
    );
    assert_ne!(
        issue_row.verification_status.to_lowercase(),
        "pending",
        "runtime heal should durably move matching specs out of pending"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_runtime_recovery_heals_missing_entity_set_map_from_matching_digest() {
    let db_path = format!(
        "/tmp/temper-test-runtime-map-heal-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");

    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let tenant_name = "test-runtime-map-heal";
    let tenant = TenantId::new(tenant_name);

    install_os_app(&state, tenant_name, "project-management")
        .await
        .expect("install should succeed");

    let bundle = get_os_app("project-management").expect("project-management app not found");
    let mut broken_csdl = bundle
        .csdl
        .clone()
        .expect("project-management should have CSDL");
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
        let mut registry = state.registry.write().unwrap();
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
    }

    let turso_ref = state.server.platform_turso_store().unwrap();

    let outcome = crate::recovery::recover_installed_app_runtime_state(
        &state,
        &turso_ref,
        tenant_name,
        "project-management",
    )
    .await;

    assert_eq!(
        outcome,
        crate::recovery::InstalledAppRuntimeRecoveryOutcome::Healed,
        "matching digest recovery should repair runtime entity-set metadata without event replay"
    );
    assert_eq!(
        state
            .registry
            .read()
            .unwrap()
            .resolve_entity_type(&tenant, "Issues")
            .as_deref(),
        Some("Issue")
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[test]
fn test_reload_picks_up_disk_changes() {
    reload_os_apps();
    let apps = list_os_apps();
    assert!(!apps.is_empty(), "catalog should not be empty after reload");
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
    assert_eq!(manifest.mode, AppDeploymentMode::Operator);
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
fn test_manifest_commons_mode_loads_commons_policy_overlay() {
    let temp_dir =
        std::env::temp_dir().join(format!("temper-app-commons-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(temp_dir.join("policies/commons")).unwrap();
    fs::write(
        temp_dir.join("app.toml"),
        r#"name = "commons-app"
description = "Commons app"
version = "1.0.0"
mode = "commons"
"#,
    )
    .unwrap();
    fs::write(
        temp_dir.join("policies/base.cedar"),
        r#"permit(principal, action == Action::"read", resource);"#,
    )
    .unwrap();
    fs::write(
        temp_dir.join("policies/commons/guardrail.cedar"),
        r#"forbid(principal, action == Action::"Create", resource);"#,
    )
    .unwrap();

    let manifest = read_app_manifest(&temp_dir).expect("manifest should parse");
    assert_eq!(manifest.mode, AppDeploymentMode::Commons);

    let bundle = load_app_bundle(&temp_dir).expect("bundle should load policies");
    assert_eq!(bundle.deployment_mode, AppDeploymentMode::Commons);
    assert_eq!(bundle.cedar_policies.len(), 2);
    let source_paths: Vec<&str> = bundle
        .cedar_policy_sources
        .iter()
        .map(|source| source.relative_path.as_str())
        .collect();
    assert_eq!(
        source_paths,
        vec!["policies/base.cedar", "policies/commons/guardrail.cedar"]
    );
    assert_eq!(
        os_app_policy_row_id("katagami-commons", "policies/palette_system.cedar"),
        "katagami-commons-palette_system"
    );
    assert_eq!(
        os_app_policy_row_id("katagami-commons", "policies/commons/guardrail.cedar"),
        "katagami-commons-commons-guardrail"
    );
    assert!(
        bundle
            .cedar_policies
            .iter()
            .any(|policy| policy.contains("forbid"))
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
    assert!(bundle.cross_invariants_toml.is_none());
    let config = bundle
        .wasm_module_configs
        .get("echo")
        .expect("wasm module config should be present");
    assert_eq!(config.startup_loading, WasmStartupLoading::Lazy);
    assert_eq!(config.criticality, WasmModuleCriticality::AppRequired);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_load_app_bundle_reads_cross_invariants() {
    let temp_dir = std::env::temp_dir().join(format!(
        "temper-cross-invariants-bundle-test-{}",
        uuid::Uuid::new_v4()
    ));
    let specs_dir = temp_dir.join("specs");
    fs::create_dir_all(&specs_dir).unwrap();

    fs::write(
        temp_dir.join("app.toml"),
        r#"name = "cross-invariants-app"
description = "Cross invariants app"
version = "1.0.0"
"#,
    )
    .unwrap();
    fs::write(
        specs_dir.join("cross-invariants.toml"),
        "version = 1\ndefault_delete_policy = \"restrict\"\n",
    )
    .unwrap();

    let bundle = load_app_bundle(&temp_dir).expect("bundle should load");
    assert_eq!(
        bundle.cross_invariants_toml.as_deref(),
        Some("version = 1\ndefault_delete_policy = \"restrict\"\n")
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_reconcile_os_app_replaces_hot_upload_when_bundled_wasm_digest_changes() {
    use temper_store_turso::TursoEventStore;

    let app_root = std::env::temp_dir().join(format!(
        "temper-os-apps-wasm-digest-reconcile-{}",
        uuid::Uuid::new_v4()
    ));
    let app_dir = app_root.join("wasm-digest-app");
    let module_dir = app_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        r#"name = "wasm-digest-app"
description = "Temporary WASM reconcile test app"
version = "0.1.0"

[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# WASM Digest App\n\nTemporary WASM reconcile test app.\n",
    )
    .unwrap();

    let echo_wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../temper-wasm/tests/fixtures/echo_integration.wasm");
    let reader_wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../temper-wasm/tests/fixtures/sdk_context_reader.wasm");
    fs::copy(&echo_wasm, module_dir.join("echo.wasm")).unwrap();
    add_os_apps_dir(app_root.clone());

    let db_path = format!(
        "/tmp/temper-wasm-digest-reconcile-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-wasm-digest-reconcile-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();
    let tenant_name = "test-wasm-digest-reconcile";
    let tenant = TenantId::new(tenant_name);

    // Re-add the temp dir before install because other concurrent catalog
    // tests can reload the global catalog and drop this synthetic app.
    add_os_apps_dir(app_root.clone());
    install_os_app(&state, tenant_name, "wasm-digest-app")
        .await
        .expect("initial app install should succeed");

    let uploaded_bytes = b"stale hot-uploaded module bytes".to_vec();
    let uploaded_hash = temper_wasm::WasmEngine::hash_module(&uploaded_bytes);
    state
        .server
        .upsert_wasm_module(
            tenant_name,
            "echo",
            &uploaded_bytes,
            &uploaded_hash,
            "upload",
        )
        .await
        .expect("hot upload should persist");
    state
        .server
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "echo", &uploaded_hash);

    fs::copy(&reader_wasm, module_dir.join("echo.wasm")).unwrap();
    add_os_apps_dir(app_root.clone());
    let expected_bundled_bytes = fs::read(module_dir.join("echo.wasm")).unwrap();
    let expected_bundled_hash = temper_wasm::WasmEngine::hash_module(&expected_bundled_bytes);

    let result = reconcile_os_app(&state, tenant_name, "wasm-digest-app")
        .await
        .expect("reconcile should succeed");
    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("changed WASM digest should reinstall the app bundle");
    };
    assert_eq!(install.wasm_modules, vec!["echo".to_string()]);

    {
        let registry = state.server.wasm_module_registry.read().unwrap();
        assert_eq!(
            registry.get_hash(&tenant, "echo"),
            Some(expected_bundled_hash.as_str())
        );
    }

    let module_sources = state
        .server
        .load_wasm_module_sources(tenant_name)
        .await
        .expect("load wasm module sources");
    let echo_source = module_sources.get("echo").expect("echo module source");
    assert_eq!(echo_source.sha256_hash.as_str(), expected_bundled_hash);
    assert_eq!(echo_source.source.as_str(), "bundled");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_dir_all(&state.server.data_dir);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_preserves_optional_hot_upload_when_bundled_wasm_digest_unchanged() {
    use temper_store_turso::TursoEventStore;

    let app_root = std::env::temp_dir().join(format!(
        "temper-os-apps-wasm-preserve-{}",
        uuid::Uuid::new_v4()
    ));
    let app_dir = app_root.join("wasm-preserve-app");
    let module_dir = app_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        r#"name = "wasm-preserve-app"
description = "Temporary WASM preserve test app"
version = "0.1.0"

[[wasm_modules]]
name = "echo"
criticality = "optional"
startup_loading = "lazy"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# WASM Preserve App\n\nTemporary WASM preserve test app.\n",
    )
    .unwrap();

    let echo_wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../temper-wasm/tests/fixtures/echo_integration.wasm");
    fs::copy(&echo_wasm, module_dir.join("echo.wasm")).unwrap();
    add_os_apps_dir(app_root.clone());

    let db_path = format!("/tmp/temper-wasm-preserve-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-wasm-preserve-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();
    let tenant_name = "test-wasm-preserve";
    let tenant = TenantId::new(tenant_name);

    // Re-add the temp dir before install because other concurrent catalog
    // tests can reload the global catalog and drop this synthetic app.
    add_os_apps_dir(app_root.clone());
    install_os_app(&state, tenant_name, "wasm-preserve-app")
        .await
        .expect("initial app install should succeed");

    let uploaded_bytes = b"same bundle hot upload should survive".to_vec();
    let uploaded_hash = temper_wasm::WasmEngine::hash_module(&uploaded_bytes);
    state
        .server
        .upsert_wasm_module(
            tenant_name,
            "echo",
            &uploaded_bytes,
            &uploaded_hash,
            "upload",
        )
        .await
        .expect("hot upload should persist");
    state
        .server
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "echo", &uploaded_hash);

    add_os_apps_dir(app_root.clone());
    let result = reconcile_os_app(&state, tenant_name, "wasm-preserve-app")
        .await
        .expect("reconcile should succeed");
    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("registry drift should run delta install");
    };
    assert!(install.wasm_modules.is_empty());
    assert_eq!(install.wasm_skipped, vec!["echo".to_string()]);

    {
        let registry = state.server.wasm_module_registry.read().unwrap();
        assert_eq!(
            registry.get_hash(&tenant, "echo"),
            Some(uploaded_hash.as_str())
        );
    }

    let module_sources = state
        .server
        .load_wasm_module_sources(tenant_name)
        .await
        .expect("load wasm module sources");
    let echo_source = module_sources.get("echo").expect("echo module source");
    assert_eq!(echo_source.sha256_hash.as_str(), uploaded_hash);
    assert_eq!(echo_source.source.as_str(), "upload");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_dir_all(&state.server.data_dir);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_replaces_stale_upload_when_app_metadata_matches_bundle() {
    use temper_store_turso::TursoEventStore;

    let app_root = std::env::temp_dir().join(format!(
        "temper-os-apps-wasm-durable-drift-{}",
        uuid::Uuid::new_v4()
    ));
    let app_dir = app_root.join("wasm-durable-drift-app");
    let module_dir = app_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        r#"name = "wasm-durable-drift-app"
description = "Temporary WASM durable drift test app"
version = "0.1.0"

[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# WASM Durable Drift App\n\nTemporary WASM durable drift test app.\n",
    )
    .unwrap();

    let echo_wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../temper-wasm/tests/fixtures/echo_integration.wasm");
    fs::copy(&echo_wasm, module_dir.join("echo.wasm")).unwrap();
    add_os_apps_dir(app_root.clone());

    let db_path = format!("/tmp/temper-wasm-durable-drift-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-wasm-durable-drift-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();
    let tenant_name = "test-wasm-durable-drift";
    let tenant = TenantId::new(tenant_name);

    add_os_apps_dir(app_root.clone());
    install_os_app(&state, tenant_name, "wasm-durable-drift-app")
        .await
        .expect("initial app install should succeed");

    let expected_bundled_bytes = fs::read(module_dir.join("echo.wasm")).unwrap();
    let expected_bundled_hash = temper_wasm::WasmEngine::hash_module(&expected_bundled_bytes);
    let current = os_app_bundle_digest("wasm-durable-drift-app").expect("durable drift app digest");
    let ps = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .expect("platform store");

    let uploaded_bytes = b"stale upload left behind by partial rollout".to_vec();
    let uploaded_hash = temper_wasm::WasmEngine::hash_module(&uploaded_bytes);
    state
        .server
        .upsert_wasm_module(
            tenant_name,
            "echo",
            &uploaded_bytes,
            &uploaded_hash,
            "upload",
        )
        .await
        .expect("stale upload should persist");

    tokio::time::sleep(Duration::from_secs(1)).await;
    ps.record_installed_app_metadata(&InstalledAppRecord {
        tenant: tenant_name.to_string(),
        app_name: current.app_name.clone(),
        source_kind: "local".to_string(),
        app_ref: String::new(),
        version_hash: String::new(),
        pinned_version_hash: String::new(),
        current_version_hash: String::new(),
        follow_policy: "pinned".to_string(),
        closure_id: String::new(),
        registry_url: String::new(),
        registry_tenant: String::new(),
        dependency_lock_digest: String::new(),
        app_version: current.app_version.clone(),
        bundle_digest: current.bundle_digest.clone(),
        spec_digest: current.spec_digest.clone(),
        policy_digest: current.policy_digest.clone(),
        wasm_digest: current.wasm_digest.clone(),
        content_digest: current.content_digest.clone(),
        seed_digest: current.seed_digest.clone(),
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    })
    .await
    .expect("installed app metadata should match bundle");

    state
        .server
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "echo", &uploaded_hash);

    add_os_apps_dir(app_root.clone());
    let result = reconcile_os_app(&state, tenant_name, "wasm-durable-drift-app")
        .await
        .expect("reconcile should succeed");
    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("durable WASM drift should run delta install");
    };
    assert_eq!(install.wasm_modules, vec!["echo".to_string()]);
    assert!(install.wasm_skipped.is_empty());

    {
        let registry = state.server.wasm_module_registry.read().unwrap();
        assert_eq!(
            registry.get_hash(&tenant, "echo"),
            Some(expected_bundled_hash.as_str())
        );
    }

    let module_sources = state
        .server
        .load_wasm_module_sources(tenant_name)
        .await
        .expect("load wasm module sources");
    let echo_source = module_sources.get("echo").expect("echo module source");
    assert_eq!(echo_source.sha256_hash.as_str(), expected_bundled_hash);
    assert_eq!(echo_source.source.as_str(), "bundled");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_dir_all(&state.server.data_dir);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_reconcile_os_app_replaces_newer_upload_for_required_module() {
    use temper_store_turso::TursoEventStore;

    let app_root = std::env::temp_dir().join(format!(
        "temper-os-apps-wasm-required-upload-{}",
        uuid::Uuid::new_v4()
    ));
    let app_dir = app_root.join("wasm-required-upload-app");
    let module_dir = app_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        r#"name = "wasm-required-upload-app"
description = "Temporary required WASM upload test app"
version = "0.1.0"

[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# Required WASM Upload App\n\nTemporary required WASM upload test app.\n",
    )
    .unwrap();

    let echo_wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../temper-wasm/tests/fixtures/echo_integration.wasm");
    fs::copy(&echo_wasm, module_dir.join("echo.wasm")).unwrap();
    add_os_apps_dir(app_root.clone());

    let db_path = format!(
        "/tmp/temper-wasm-required-upload-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-wasm-required-upload-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();
    let tenant_name = "test-wasm-required-upload";
    let tenant = TenantId::new(tenant_name);

    add_os_apps_dir(app_root.clone());
    install_os_app(&state, tenant_name, "wasm-required-upload-app")
        .await
        .expect("initial app install should succeed");

    let expected_bundled_bytes = fs::read(module_dir.join("echo.wasm")).unwrap();
    let expected_bundled_hash = temper_wasm::WasmEngine::hash_module(&expected_bundled_bytes);
    let current =
        os_app_bundle_digest("wasm-required-upload-app").expect("required upload app digest");
    let ps = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .expect("platform store");
    ps.record_installed_app_metadata(&InstalledAppRecord {
        tenant: tenant_name.to_string(),
        app_name: current.app_name.clone(),
        source_kind: "local".to_string(),
        app_ref: String::new(),
        version_hash: String::new(),
        pinned_version_hash: String::new(),
        current_version_hash: String::new(),
        follow_policy: "pinned".to_string(),
        closure_id: String::new(),
        registry_url: String::new(),
        registry_tenant: String::new(),
        dependency_lock_digest: String::new(),
        app_version: current.app_version.clone(),
        bundle_digest: current.bundle_digest.clone(),
        spec_digest: current.spec_digest.clone(),
        policy_digest: current.policy_digest.clone(),
        wasm_digest: current.wasm_digest.clone(),
        content_digest: current.content_digest.clone(),
        seed_digest: current.seed_digest.clone(),
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    })
    .await
    .expect("installed app metadata should match bundle");

    tokio::time::sleep(Duration::from_secs(1)).await;
    let uploaded_bytes = b"newer stale upload shadowing required module".to_vec();
    let uploaded_hash = temper_wasm::WasmEngine::hash_module(&uploaded_bytes);
    state
        .server
        .upsert_wasm_module(
            tenant_name,
            "echo",
            &uploaded_bytes,
            &uploaded_hash,
            "upload",
        )
        .await
        .expect("newer stale upload should persist");
    state
        .server
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "echo", &uploaded_hash);

    add_os_apps_dir(app_root.clone());
    let result = reconcile_os_app(&state, tenant_name, "wasm-required-upload-app")
        .await
        .expect("reconcile should succeed");
    let OsAppReconcileResult::Installed { install, .. } = result else {
        panic!("required WASM upload drift should run delta install");
    };
    assert_eq!(install.wasm_modules, vec!["echo".to_string()]);
    assert!(install.wasm_skipped.is_empty());

    {
        let registry = state.server.wasm_module_registry.read().unwrap();
        assert_eq!(
            registry.get_hash(&tenant, "echo"),
            Some(expected_bundled_hash.as_str())
        );
    }

    let module_sources = state
        .server
        .load_wasm_module_sources(tenant_name)
        .await
        .expect("load wasm module sources");
    let echo_source = module_sources.get("echo").expect("echo module source");
    assert_eq!(echo_source.sha256_hash.as_str(), expected_bundled_hash);
    assert_eq!(echo_source.source.as_str(), "bundled");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_dir_all(&state.server.data_dir);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

#[test]
fn test_required_wasm_config_without_artifact_is_not_registered_ready() {
    let mut configs = BTreeMap::new();
    configs.insert(
        "echo".to_string(),
        WasmModuleManifest {
            name: "echo".to_string(),
            target: None,
            criticality: WasmModuleCriticality::AppRequired,
            startup_loading: WasmStartupLoading::Lazy,
            provenance: None,
            import_class: None,
            data: None,
            data_binding: None,
        },
    );
    let bundle = AppBundle {
        deployment_mode: AppDeploymentMode::Operator,
        specs: Vec::new(),
        csdl: None,
        cross_invariants_toml: None,
        cedar_policies: Vec::new(),
        cedar_policy_sources: Vec::new(),
        wasm_modules: BTreeMap::new(),
        wasm_module_configs: configs,
        agents: Vec::new(),
        skills: Vec::new(),
        adrs: Vec::new(),
        system_files: Vec::new(),
        seed_instances: Vec::new(),
    };
    let state = PlatformState::new(None);

    assert!(
        !tenant_has_registered_wasm_for_bundle(&state, "test-required-missing-artifact", &bundle),
        "configured required WASM modules without bundled bytes must not be treated as ready"
    );
}

#[test]
fn test_load_app_bundle_rejects_legacy_reactions_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "temper-legacy-reactions-bundle-test-{}",
        uuid::Uuid::new_v4()
    ));
    let reactions_dir = temp_dir.join("reactions");
    fs::create_dir_all(&reactions_dir).unwrap();

    fs::write(
        temp_dir.join("app.toml"),
        r#"name = "legacy-reactions-app"
description = "Legacy reactions app"
version = "1.0.0"
"#,
    )
    .unwrap();
    fs::write(
        reactions_dir.join("reactions.toml"),
        r#"[[reaction]]
name = "legacy"
[reaction.when]
entity_type = "File"
action = "StreamUpdated"
[reaction.then]
entity_type = "FileVersion"
action = "Create"
[reaction.resolve_target]
type = "field"
field = "file_id"
"#,
    )
    .unwrap();

    let bundle = load_app_bundle(&temp_dir);
    assert!(
        bundle.is_none(),
        "legacy reactions.toml should be rejected after the ADR-0046 hard cut"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_find_wasm_modules_respects_manifest_target() {
    // When the manifest declares `target = "wasm32-wasip1"`, the discovery
    // function must pick the wasip1 binary even if a wasm32-unknown-unknown
    // binary also exists (e.g. a stale build from a different compilation).
    let temp_dir =
        std::env::temp_dir().join(format!("temper-wasm-target-test-{}", uuid::Uuid::new_v4()));

    // Create both target directories with different content.
    let uu_dir = temp_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release");
    let wasip1_dir = temp_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-wasip1")
        .join("release");
    fs::create_dir_all(&uu_dir).unwrap();
    fs::create_dir_all(&wasip1_dir).unwrap();

    let wrong_bytes = b"wrong-target-binary";
    let correct_bytes = b"correct-wasip1-binary";
    fs::write(uu_dir.join("echo.wasm"), wrong_bytes).unwrap();
    fs::write(wasip1_dir.join("echo.wasm"), correct_bytes).unwrap();

    // Build module configs with a declared target.
    let mut configs = BTreeMap::new();
    configs.insert(
        "echo".to_string(),
        WasmModuleManifest {
            name: "echo".to_string(),
            target: Some("wasm32-wasip1".to_string()),
            criticality: WasmModuleCriticality::default(),
            startup_loading: WasmStartupLoading::default(),
            provenance: None,
            import_class: None,
            data: None,
            data_binding: None,
        },
    );

    let modules = find_wasm_modules(&temp_dir, &configs);
    assert!(modules.contains_key("echo"), "echo module should be found");
    assert_eq!(
        modules["echo"], correct_bytes,
        "should pick the wasip1 binary, not wasm32-unknown-unknown"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_find_wasm_modules_prefers_packaged_sibling_over_target_output() {
    let temp_dir = std::env::temp_dir().join(format!(
        "temper-wasm-packaged-test-{}",
        uuid::Uuid::new_v4()
    ));

    let target_dir = temp_dir
        .join("wasm")
        .join("echo")
        .join("target")
        .join("wasm32-wasip1")
        .join("release");
    let module_dir = temp_dir.join("wasm").join("echo");
    fs::create_dir_all(&target_dir).unwrap();

    let stale_target_bytes = b"stale-target-output";
    let packaged_bytes = b"packaged-sibling-artifact";
    fs::write(target_dir.join("echo.wasm"), stale_target_bytes).unwrap();
    fs::write(module_dir.join("echo.wasm"), packaged_bytes).unwrap();

    let mut configs = BTreeMap::new();
    configs.insert(
        "echo".to_string(),
        WasmModuleManifest {
            name: "echo".to_string(),
            target: Some("wasm32-wasip1".to_string()),
            criticality: WasmModuleCriticality::AppRequired,
            startup_loading: WasmStartupLoading::default(),
            provenance: None,
            import_class: None,
            data: None,
            data_binding: None,
        },
    );

    let modules = find_wasm_modules(&temp_dir, &configs);
    assert_eq!(
        modules["echo"], packaged_bytes,
        "packaged sibling artifact should win over target output"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_find_wasm_modules_finds_sibling_artifact() {
    // When no target/ directories exist, the discovery function should find
    // a sibling bundled artifact at {module_name}/{module_name}.wasm — this
    // is where build.sh copies compiled modules for deployment.
    let temp_dir =
        std::env::temp_dir().join(format!("temper-wasm-sibling-test-{}", uuid::Uuid::new_v4()));

    let module_dir = temp_dir.join("wasm").join("echo");
    fs::create_dir_all(&module_dir).unwrap();

    let artifact_bytes = b"sibling-artifact-binary";
    fs::write(module_dir.join("echo.wasm"), artifact_bytes).unwrap();

    let mut configs = BTreeMap::new();
    configs.insert(
        "echo".to_string(),
        WasmModuleManifest {
            name: "echo".to_string(),
            target: None,
            criticality: WasmModuleCriticality::default(),
            startup_loading: WasmStartupLoading::default(),
            provenance: None,
            import_class: None,
            data: None,
            data_binding: None,
        },
    );

    let modules = find_wasm_modules(&temp_dir, &configs);
    assert!(
        modules.contains_key("echo"),
        "echo module should be found at sibling path"
    );
    assert_eq!(modules["echo"], artifact_bytes);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_find_wasm_modules_ignores_undeclared_artifact() {
    let temp_dir = std::env::temp_dir().join(format!(
        "temper-wasm-undeclared-test-{}",
        uuid::Uuid::new_v4()
    ));

    let module_dir = temp_dir.join("wasm").join("echo");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(module_dir.join("echo.wasm"), b"undeclared").unwrap();

    let modules = find_wasm_modules(&temp_dir, &BTreeMap::new());
    assert!(
        modules.is_empty(),
        "WASM artifacts must be declared in app.toml before install"
    );

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

#[test]
fn test_agent_soul_refresh_decision_preserves_customized_content() {
    let desired_hash = content_sha256(b"expected agent content");

    assert_eq!(
        decide_agent_soul_refresh(false, "", &desired_hash),
        AgentSoulRefreshDecision::Upload
    );
    assert_eq!(
        decide_agent_soul_refresh(true, &desired_hash, &desired_hash),
        AgentSoulRefreshDecision::AlreadyCurrent
    );
    assert_eq!(
        decide_agent_soul_refresh(true, "sha256:someone-customized-this", &desired_hash),
        AgentSoulRefreshDecision::PreserveCustomized
    );
}

#[test]
fn test_state_field_str_accepts_lowercase_names() {
    let mut fields = serde_json::Map::new();
    fields.insert("name".to_string(), serde_json::json!("paw"));
    let fields = serde_json::Value::Object(fields);

    assert_eq!(state_field_str(&fields, &["Name", "name"]), Some("paw"));
}

#[tokio::test]
async fn test_install_app_bootstraps_adrs_into_temper_fs() {
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
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-adr-test-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();

    // Re-add the temp dir before each install — the concurrent
    // `test_reload_picks_up_disk_changes` test calls `reload_os_apps()`
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

#[tokio::test]
async fn test_install_app_skill_docs_are_query_readable_by_canonical_path_fields() {
    use temper_store_turso::TursoEventStore;

    let app_root =
        std::env::temp_dir().join(format!("temper-skill-doc-app-{}", uuid::Uuid::new_v4()));
    let app_dir = app_root.join("skill-doc-app");
    fs::create_dir_all(app_dir.join("agents/curator/skills/research-direction")).unwrap();
    fs::create_dir_all(app_dir.join("specs")).unwrap();
    fs::write(
        app_dir.join("app.toml"),
        "name = \"skill-doc-app\"\ndescription = \"Temporary skill doc app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# Skill Doc App\n\nTemporary skill doc app.\n",
    )
    .unwrap();
    fs::write(app_dir.join("agents/curator/AGENT.md"), "# Curator\n").unwrap();
    fs::write(
        app_dir.join("agents/curator/skills/research-direction/SKILL.md"),
        "# Research Direction\n\nFind useful source material.\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("specs/soul.ioa.toml"),
        r#"
[automaton]
name = "Soul"
states = ["Created", "Published"]
initial = "Created"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["name", "description", "content_file_id"]

[[action]]
name = "Publish"
kind = "input"
from = ["Created", "Published"]
to = "Published"
"#,
    )
    .unwrap();

    let db_path = format!("/tmp/temper-skill-doc-test-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-skill-doc-test-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();

    add_os_apps_dir(app_root.clone());
    install_os_app(&state, "test-skill-doc-app", "temper-fs")
        .await
        .expect("install temper-fs");
    add_os_apps_dir(app_root.clone());
    install_os_app(&state, "test-skill-doc-app", "skill-doc-app")
        .await
        .expect("install skill-doc-app");

    let tenant = TenantId::new("test-skill-doc-app");
    let agent_id = bootstrapped_agent_soul_entity_id("curator");
    let file_id = format!(
        "os-agent-skill-file-{}-research-direction",
        slug_fragment(&agent_id)
    );
    let expected_path = format!("/agents/{agent_id}/skills/research-direction/SKILL.md");
    let file = state
        .server
        .get_tenant_entity_state(&tenant, "File", &file_id)
        .await
        .expect("skill file should exist");

    assert_eq!(file.state.status, "Ready");
    assert_eq!(
        file.state
            .fields
            .get("path")
            .and_then(|value| value.as_str()),
        Some(expected_path.as_str())
    );
    assert_eq!(
        file.state
            .fields
            .get("Path")
            .and_then(|value| value.as_str()),
        Some(expected_path.as_str())
    );
    assert_eq!(
        file.state
            .fields
            .get("workspace_id")
            .and_then(|value| value.as_str()),
        Some(APP_DOCS_WORKSPACE_ID)
    );
    assert_eq!(
        file.state
            .fields
            .get("WorkspaceId")
            .and_then(|value| value.as_str()),
        Some(APP_DOCS_WORKSPACE_ID)
    );

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path}-wal"));
    let _ = fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_ensure_markdown_file_repairs_existing_canonical_query_aliases() {
    use temper_store_turso::TursoEventStore;

    let db_path = format!(
        "/tmp/temper-skill-doc-repair-test-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-skill-doc-repair-test-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();

    install_os_app(&state, "test-skill-doc-repair", "temper-fs")
        .await
        .expect("install temper-fs");

    let tenant = TenantId::new("test-skill-doc-repair");
    let agent_ctx = AgentContext::for_service("test-bootstrap");
    let path = "/agents/sl-bootstrap-agent-soul-curator/skills/research-direction/SKILL.md";
    let file_id = "os-agent-skill-file-sl-bootstrap-agent-soul-curator-research-direction";
    state
        .server
        .create_file_with_initial_stream_content(
            &tenant,
            file_id,
            serde_json::json!({
                "name": "SKILL.md",
                "path": path,
                "directory_id": "os-agent-skill-dir-sl-bootstrap-agent-soul-curator-research-direction",
                "workspace_id": APP_DOCS_WORKSPACE_ID,
                "mime_type": "text/markdown",
            }),
            b"# Research Direction\n",
            "text/markdown",
            &agent_ctx,
        )
        .await
        .expect("create legacy lower-case-only skill file");

    let before = state
        .server
        .get_tenant_entity_state(&tenant, "File", file_id)
        .await
        .expect("legacy file should exist");
    assert!(
        before.state.fields.get("Path").is_none(),
        "test setup should match lower-case-only legacy projection"
    );

    ensure_markdown_file(
        &state,
        &tenant,
        &agent_ctx,
        MarkdownFileBootstrapTarget {
            file_id,
            name: "SKILL.md",
            path,
            directory_id: "os-agent-skill-dir-sl-bootstrap-agent-soul-curator-research-direction",
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
        b"# Research Direction\n",
    )
    .await
    .expect("repair skill file aliases");

    let after = state
        .server
        .get_tenant_entity_state(&tenant, "File", file_id)
        .await
        .expect("repaired file should exist");
    assert_eq!(
        after
            .state
            .fields
            .get("Path")
            .and_then(|value| value.as_str()),
        Some(path)
    );
    assert_eq!(
        after
            .state
            .fields
            .get("WorkspaceId")
            .and_then(|value| value.as_str()),
        Some(APP_DOCS_WORKSPACE_ID)
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path}-wal"));
    let _ = fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_install_os_app_rebuilds_reaction_dispatcher_for_inline_entity_triggers() {
    use serde_json::json;
    use temper_runtime::tenant::TenantId;
    use temper_server::request_context::AgentContext;

    let app_root = std::env::temp_dir().join(format!(
        "temper-inline-trigger-install-{}",
        uuid::Uuid::new_v4()
    ));
    let app_dir = app_root.join("inline-trigger-app");
    fs::create_dir_all(app_dir.join("specs")).unwrap();
    fs::create_dir_all(app_dir.join("policies")).unwrap();

    fs::write(
        app_dir.join("app.toml"),
        "name = \"inline-trigger-app\"\ndescription = \"Temporary inline trigger test app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("APP.md"),
        "# Inline Trigger App\n\nTemporary inline trigger install test app.\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("specs/model.csdl.xml"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.InstallTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="PaymentId" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Payment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.InstallTest.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.InstallTest.Payment"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("specs/order.ioa.toml"),
        r#"[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed"]
initial = "Draft"

[[state]]
name = "payment_id"
type = "string"
initial = ""

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
params = ["payment_id"]

[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
kind = "internal"
from = ["Submitted"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("specs/payment.ioa.toml"),
        r#"[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
kind = "internal"
from = ["Pending"]
to = "Authorized"
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("policies/payment.cedar"),
        r#"permit(
    principal is Agent,
    action == Action::"AuthorizePayment",
    resource is Payment
) when {
    principal.agent_type == "payment-service"
};
"#,
    )
    .unwrap();

    add_os_apps_dir(app_root.clone());

    let db_path = format!("/tmp/temper-inline-reaction-{}.db", uuid::Uuid::new_v4());
    let turso = temper_store_turso::TursoEventStore::new(&format!("file:{db_path}"), None)
        .await
        .expect("create durable reaction test store");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    add_os_apps_dir(app_root.clone());
    install_os_app(&state, "test-inline-trigger", "inline-trigger-app")
        .await
        .expect("install inline-trigger-app");

    let tenant = TenantId::new("test-inline-trigger");
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-1",
            "AddItem",
            json!({"payment_id":"pay-1"}),
            &AgentContext::system(),
        )
        .await
        .expect("AddItem should succeed");
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-1",
            "SubmitOrder",
            json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("SubmitOrder should succeed");
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-1",
            "ConfirmOrder",
            json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("ConfirmOrder should succeed");

    tokio::task::yield_now().await;

    let payment = state
        .server
        .get_tenant_entity_state(&tenant, "Payment", "pay-1")
        .await
        .expect("payment should exist after inline trigger dispatch");
    assert_eq!(payment.state.status, "Authorized");

    let _ = fs::remove_dir_all(&app_root);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path}-wal"));
    let _ = fs::remove_file(format!("{db_path}-shm"));
}

#[tokio::test]
async fn test_local_stream_uploads_create_real_file_version_lineage() {
    use temper_server::state::DispatchCommand;
    use temper_store_turso::TursoEventStore;

    let db_path = format!(
        "/tmp/temper-file-version-lineage-{}.db",
        uuid::Uuid::new_v4()
    );
    let db_url = format!("file:{db_path}");
    let turso = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    state.server.data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-file-version-lineage-{}-data",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&state.server.data_dir).unwrap();

    install_os_app(&state, "test-file-lineage", "temper-fs")
        .await
        .expect("install temper-fs");

    let tenant = TenantId::new("test-file-lineage");
    let agent_ctx = AgentContext::system();
    let file_id = "file-version-lineage";

    state
        .server
        .get_or_create_tenant_entity(&tenant, "File", file_id, serde_json::json!({}))
        .await
        .expect("create file actor");
    state
        .server
        .dispatch(DispatchCommand {
            tenant: &tenant,
            entity_type: "File",
            entity_id: file_id,
            action: "Create",
            params: serde_json::json!({
                "name": "lineage.txt",
                "path": "/lineage.txt",
                "directory_id": "",
                "workspace_id": "",
                "mime_type": "text/plain",
            }),
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
        .expect("initialize file");

    state
        .server
        .put_file_stream_content(&tenant, file_id, b"first version", "text/plain", &agent_ctx)
        .await
        .expect("upload first version");

    let mut first_version_id = String::new();
    for _ in 0..200 {
        let file_after_first_upload = state
            .server
            .get_tenant_entity_state(&tenant, "File", file_id)
            .await
            .expect("load file state after first upload");
        if let Some(version_id) = file_after_first_upload
            .state
            .fields
            .get("last_version_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            first_version_id = version_id.to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !first_version_id.is_empty(),
        "first upload should populate last_version_id before second upload"
    );

    state
        .server
        .put_file_stream_content(
            &tenant,
            file_id,
            b"second version",
            "text/plain",
            &agent_ctx,
        )
        .await
        .expect("upload second version");

    let mut file_resp = None;
    for _ in 0..200 {
        let candidate = state
            .server
            .get_tenant_entity_state(&tenant, "File", file_id)
            .await
            .expect("load file state");
        let latest_version_id = candidate
            .state
            .fields
            .get("last_version_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if candidate.state.counters.get("version_count") == Some(&2)
            && !latest_version_id.is_empty()
            && latest_version_id != first_version_id
        {
            file_resp = Some(candidate);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let file_resp =
        file_resp.expect("file should point at the second version after trigger cascade");
    assert_eq!(file_resp.state.status, "Ready");
    assert_eq!(file_resp.state.counters.get("version_count"), Some(&2));

    let latest_version_id = file_resp
        .state
        .fields
        .get("last_version_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .expect("file should point at latest version")
        .to_string();

    let latest_version = state
        .server
        .get_tenant_entity_state(&tenant, "FileVersion", &latest_version_id)
        .await
        .expect("load latest file version");
    assert_eq!(latest_version.state.status, "Current");
    assert_eq!(
        latest_version.state.counters.get("version_number"),
        Some(&2)
    );

    let previous_version_id = latest_version
        .state
        .fields
        .get("previous_version_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .expect("latest version should point at previous version")
        .to_string();

    for _ in 0..200 {
        let previous = state
            .server
            .get_tenant_entity_state(&tenant, "FileVersion", &previous_version_id)
            .await
            .expect("load previous file version");
        if previous.state.status == "Superseded" {
            assert_eq!(previous.state.counters.get("version_number"), Some(&1));
            let _ = fs::remove_file(&db_path);
            let _ = fs::remove_file(format!("{db_path}-wal"));
            let _ = fs::remove_file(format!("{db_path}-shm"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let previous = state
        .server
        .get_tenant_entity_state(&tenant, "FileVersion", &previous_version_id)
        .await
        .expect("load previous file version after wait");
    assert_eq!(
        previous.state.status, "Superseded",
        "previous version should be superseded after the second upload"
    );
    assert_eq!(previous.state.counters.get("version_number"), Some(&1));

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path}-wal"));
    let _ = fs::remove_file(format!("{db_path}-shm"));
}
