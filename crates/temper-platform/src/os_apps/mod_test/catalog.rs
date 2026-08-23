use super::*;
#[test]
fn test_directed_evolution_specs_parse() {
    let bundle = get_os_app("directed-evolution").expect("directed-evolution app not found");
    assert_eq!(bundle.specs.len(), 26);
    for (entity_type, ioa_source) in &bundle.specs {
        let result = automaton::parse_automaton(ioa_source);
        assert!(
            result.is_ok(),
            "Directed Evolution spec {} failed to parse: {:?}",
            entity_type,
            result.err()
        );
    }
}

#[test]
fn test_directed_evolution_csdl_parses() {
    let bundle = get_os_app("directed-evolution").expect("directed-evolution app not found");
    let result = parse_csdl(
        bundle
            .csdl
            .as_ref()
            .expect("directed-evolution should have CSDL"),
    );
    assert!(
        result.is_ok(),
        "Directed Evolution CSDL failed to parse: {:?}",
        result.err()
    );
}

#[test]
fn test_directed_evolution_specs_verify() {
    let bundle = get_os_app("directed-evolution").expect("directed-evolution app not found");
    for (entity_type, ioa_source) in &bundle.specs {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(2)
            .with_prop_test_cases(20);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "Directed Evolution spec {} failed verification",
            entity_type
        );
    }
}

#[tokio::test]
async fn test_install_os_app_directed_evolution_registers_entities() {
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-directed-evolution", "directed-evolution").await;
    assert!(result.is_ok(), "install failed: {:?}", result.err());
    let result = result.expect("directed evolution app installs");
    assert_eq!(
        result.added.len(),
        26,
        "expected 26 added: {:?}",
        result.added
    );
    assert!(result.updated.is_empty());
    assert!(result.skipped.is_empty());
    assert!(result.added.contains(&"Organism".to_string()));
    assert!(result.added.contains(&"Direction".to_string()));
    assert!(result.added.contains(&"Episode".to_string()));
    assert!(result.added.contains(&"EpisodeStartRequest".to_string()));
    assert!(result.added.contains(&"Variant".to_string()));
    assert!(result.added.contains(&"StageResult".to_string()));
    assert!(result.added.contains(&"Trial".to_string()));
    assert!(result.added.contains(&"Promotion".to_string()));
    assert!(result.added.contains(&"WorkItem".to_string()));
    assert!(result.added.contains(&"BrainRun".to_string()));
    assert_eq!(
        result.wasm_modules,
        vec![
            "episode_orchestrator".to_string(),
            "episode_start_requestor".to_string(),
            "signal_observer".to_string(),
            "work_item_result_router".to_string(),
        ]
    );

    let registry = state
        .registry
        .read()
        .expect("registry lock is not poisoned");
    let tenant = TenantId::new("test-directed-evolution");
    assert!(registry.get_table(&tenant, "Organism").is_some());
    assert!(registry.get_table(&tenant, "Direction").is_some());
    assert!(registry.get_table(&tenant, "Episode").is_some());
    assert!(registry.get_table(&tenant, "EpisodeStartRequest").is_some());
    assert!(registry.get_table(&tenant, "Variant").is_some());
    assert!(registry.get_table(&tenant, "StageResult").is_some());
    assert!(registry.get_table(&tenant, "Trial").is_some());
    assert!(registry.get_table(&tenant, "Promotion").is_some());
    assert!(registry.get_table(&tenant, "WorkItem").is_some());
    assert!(registry.get_table(&tenant, "BrainRun").is_some());
}

#[tokio::test]
async fn test_directed_evolution_policy_admits_only_packaged_wasm_modules() {
    let state = PlatformState::new(None);
    let tenant = "test-directed-evolution-wasm-policy";
    install_os_app(&state, tenant, "directed-evolution")
        .await
        .expect("directed-evolution app installs");

    let module_context = |module_name: &str, role: &str| temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: module_name.to_string(),
            kind: temper_authz::PrincipalKind::Agent,
            role: Some(role.to_string()),
            acting_for: None,
            agent_type: None,
            attributes: std::collections::HashMap::new(),
        },
        context_attrs: std::collections::HashMap::new(),
        correlation_id: "directed-evolution-wasm-policy-test".to_string(),
    };
    let attrs = std::collections::BTreeMap::new();

    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("signal_observer", "wasm_module"),
                "create",
                "WorkItem",
                &attrs,
                tenant,
            )
            .is_ok(),
        "a packaged WASM module should receive the app's action/resource permits"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("signal_observer", "wasm_module"),
                "create",
                "Episode",
                &attrs,
                tenant,
            )
            .is_err(),
        "signal_observer must not create entities outside its WorkItem contract"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("signal_observer", "wasm_module"),
                "PromoteWinner",
                "Promotion",
                &attrs,
                tenant,
            )
            .is_err(),
        "signal_observer must not inherit another module's action grants"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("episode_orchestrator", "wasm_module"),
                "create",
                "Generation",
                &attrs,
                tenant,
            )
            .is_ok(),
        "episode_orchestrator should create generations"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("episode_orchestrator", "wasm_module"),
                "create",
                "Project",
                &attrs,
                tenant,
            )
            .is_err(),
        "a Directed Evolution module must not create cross-app entities"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("work_item_result_router", "wasm_module"),
                "PromoteWinner",
                "Promotion",
                &attrs,
                tenant,
            )
            .is_ok(),
        "work_item_result_router should retain its promotion contract"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("unregistered_module", "wasm_module"),
                "create",
                "WorkItem",
                &attrs,
                tenant,
            )
            .is_err(),
        "an unknown WASM module must remain denied"
    );
    assert!(
        state
            .server
            .authorize_with_context(
                &module_context("signal_observer", "operator"),
                "create",
                "WorkItem",
                &attrs,
                tenant,
            )
            .is_err(),
        "a known module id without the host-derived role must remain denied"
    );
}
