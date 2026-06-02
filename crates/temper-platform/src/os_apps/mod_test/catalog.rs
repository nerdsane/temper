use super::*;
#[test]
fn test_directed_evolution_specs_parse() {
    if skip_without_genesis_apps("test_directed_evolution_specs_parse") {
        return;
    }
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
    if skip_without_genesis_apps("test_directed_evolution_csdl_parses") {
        return;
    }
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
    if skip_without_genesis_apps("test_directed_evolution_specs_verify") {
        return;
    }
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
    if skip_without_genesis_apps("test_install_os_app_directed_evolution_registers_entities") {
        return;
    }
    let state = PlatformState::new(None);
    let result = install_os_app(&state, "test-directed-evolution", "directed-evolution").await;
    assert!(result.is_ok(), "install failed: {:?}", result.err());
    let result = result.expect("directed evolution app installs");
    assert_eq!(
        result.added.len(),
        26,
        "expected 26 directed-evolution additions after dependencies: {:?}",
        result.added
    );
    assert!(
        result.updated.is_empty(),
        "unexpected updates: {:?}",
        result.updated
    );
    assert!(result.skipped.is_empty());
    assert!(result.added.contains(&"Organism".to_string()));
    assert!(result.added.contains(&"Direction".to_string()));
    assert!(result.added.contains(&"Episode".to_string()));
    assert!(result.added.contains(&"Variant".to_string()));
    assert!(result.added.contains(&"StageResult".to_string()));
    assert!(result.added.contains(&"Trial".to_string()));
    assert!(result.added.contains(&"Promotion".to_string()));
    assert!(result.added.contains(&"WorkItemReceipt".to_string()));
    assert_eq!(
        result.wasm_modules,
        vec![
            "episode_orchestrator".to_string(),
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
    assert!(registry.get_table(&tenant, "Variant").is_some());
    assert!(registry.get_table(&tenant, "StageResult").is_some());
    assert!(registry.get_table(&tenant, "Trial").is_some());
    assert!(registry.get_table(&tenant, "Promotion").is_some());
    assert!(registry.get_table(&tenant, "WorkItemReceipt").is_some());
    assert!(registry.get_table(&tenant, "WorkItem").is_some());
    assert!(registry.get_table(&tenant, "WorkerRun").is_some());
    assert!(registry.get_table(&tenant, "WorkerProvider").is_some());
    assert!(registry.get_table(&tenant, "WorkerAgent").is_some());
}
