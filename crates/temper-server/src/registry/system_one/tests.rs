use super::*;
use crate::registry::SpecRegistry;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices>
<Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Case"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.Guid"/>
<Property Name="Messages" Type="Edm.String"/></EntityType>
<EntityContainer Name="Service"><EntitySet Name="Cases" EntityType="Test.Case"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn spec(reference: &str) -> String {
    format!(
        r#"
[automaton]
name = "Case"
states = ["Open", "Escalated"]
initial = "Open"
[[state]]
name = "attempts"
type = "counter"
initial = "0"
[[state]]
name = "approved"
type = "bool"
initial = "false"
[[state]]
name = "labels"
type = "set"
initial = "[]"
[[action]]
name = "Escalate"
from = ["Open"]
to = "Escalated"
params = ["Message"]
guard = [{{ type = "system_one", model = "jev-latest", state = {{ context = {{ ref = "{reference}" }} }}, questions = {{ human = {{ type = "noul", instructions = "Human requested?" }} }}, assert = "answers.human.noul >= 0.8" }}]
"#
    )
}

#[test]
fn declared_fields_state_variables_and_params_are_valid_sources() {
    for reference in [
        "entity.Messages",
        "entity.Status",
        "entity.Id",
        "entity.attempts",
        "entity.approved",
        "entity.labels",
        "params.Message",
    ] {
        let mut registry = SpecRegistry::new();
        registry
            .try_register_tenant(
                "alpha",
                parse_csdl(CSDL).unwrap(),
                CSDL.into(),
                &[("Case", &spec(reference))],
            )
            .unwrap();
    }
}

#[test]
fn unknown_inputs_reject_registration_before_publishing() {
    for reference in [
        "entity.Messsages",
        "params.Unknown",
        "entity.has_spec",
        "entity.HasSpec",
    ] {
        let mut registry = SpecRegistry::new();
        let csdl = CSDL.replace(
            "<Property Name=\"Messages\" Type=\"Edm.String\"/>",
            "<Property Name=\"Messages\" Type=\"Edm.String\"/><Property Name=\"has_spec\" Type=\"Edm.Boolean\"/><Property Name=\"HasSpec\" Type=\"Edm.Boolean\"/>",
        );
        let result = registry.try_register_tenant(
            "alpha",
            parse_csdl(&csdl).unwrap(),
            csdl,
            &[("Case", &spec(reference))],
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("undeclared system_one state reference")
        );
        assert!(registry.get_tenant(&TenantId::new("alpha")).is_none());
    }
}

#[test]
fn invalid_hot_reload_does_not_mutate_existing_config_or_table() {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "alpha",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Case", &spec("entity.Messages"))],
        )
        .unwrap();
    let tenant = TenantId::new("alpha");
    let before = registry
        .get_spec(&tenant, "Case")
        .unwrap()
        .ioa_source
        .clone();
    let changed_csdl = CSDL.replace("Name=\"Messages\"", "Name=\"Other\"");
    let result = registry.try_register_tenant(
        "alpha",
        parse_csdl(&changed_csdl).unwrap(),
        changed_csdl,
        &[("Case", &spec("entity.Unknown"))],
    );
    assert!(result.is_err());
    let config = registry.get_tenant(&tenant).unwrap();
    assert_eq!(config.csdl_xml.as_str(), CSDL);
    assert_eq!(
        registry.get_spec(&tenant, "Case").unwrap().ioa_source,
        before
    );
}

#[test]
fn merged_deployments_resolve_existing_entity_contracts() {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "alpha",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Case", &spec("entity.Messages"))],
        )
        .unwrap();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            CsdlDocument {
                version: "4.0".into(),
                schemas: Vec::new(),
            },
            String::new(),
            &[("Case", &spec("entity.Messages"))],
            Vec::new(),
            None,
            true,
        )
        .unwrap();
}

#[test]
fn csdl_only_merge_cannot_remove_input_used_by_retained_guard() {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "alpha",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Case", &spec("entity.Messages"))],
        )
        .unwrap();
    let changed = CSDL.replace("Name=\"Messages\"", "Name=\"Other\"");
    let result = registry.try_register_tenant_with_reactions_and_constraints(
        "alpha",
        parse_csdl(&changed).unwrap(),
        changed,
        &[],
        Vec::new(),
        None,
        true,
    );
    assert!(result.is_err());
    assert_eq!(
        registry
            .get_tenant(&TenantId::new("alpha"))
            .unwrap()
            .csdl_xml
            .as_str(),
        CSDL
    );
}
