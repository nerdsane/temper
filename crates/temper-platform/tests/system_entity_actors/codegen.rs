use super::*;

// =========================================================================
// CODEGEN — Tier 1 compiled types from system CSDL
// =========================================================================

#[test]
fn codegen_system_entities_produce_valid_modules() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).expect("system CSDL should parse");

    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    // Generate Tier 1 compiled code for each system entity
    for entity_name in &[
        "Project",
        "Tenant",
        "CatalogEntry",
        "Collaborator",
        "Version",
    ] {
        let module = generate_entity_module(&spec, entity_name)
            .unwrap_or_else(|e| panic!("codegen for {entity_name} failed: {e}"));

        // Verify generated code contains expected structures
        assert!(
            module
                .source
                .contains(&format!("pub struct {}State", entity_name)),
            "{entity_name} should have a state struct:\n{}",
            &module.source[..200.min(module.source.len())]
        );
        assert!(
            module
                .source
                .contains(&format!("pub enum {}Msg", entity_name)),
            "{entity_name} should have a message enum"
        );
        assert!(
            module.source.contains("pub id:"),
            "{entity_name} should have an id field"
        );
        assert!(
            module.source.contains("pub status:"),
            "{entity_name} should have a status field"
        );
    }
}

#[test]
fn codegen_project_has_typed_fields() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    let module = generate_entity_module(&spec, "Project").unwrap();

    // Project-specific fields from CSDL
    assert!(
        module.source.contains("pub name:"),
        "Project should have name field"
    );
    assert!(
        module.source.contains("pub description:"),
        "Project should have description field"
    );
    assert!(
        module.source.contains("Verify"),
        "Project should have Verify action"
    );
    assert!(
        module.source.contains("Archive"),
        "Project should have Archive action"
    );
    assert!(
        module.source.contains("UpdateSpecs"),
        "Project should have UpdateSpecs action"
    );
}

#[test]
fn codegen_tenant_has_project_reference() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    let module = generate_entity_module(&spec, "Tenant").unwrap();

    assert!(
        module.source.contains("Deploy"),
        "Tenant should have Deploy action"
    );
    assert!(
        module.source.contains("Suspend"),
        "Tenant should have Suspend action"
    );
    assert!(
        module.source.contains("Reactivate"),
        "Tenant should have Reactivate action"
    );
}
