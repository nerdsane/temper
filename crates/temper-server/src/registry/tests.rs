use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{CsdlDocument, parse_csdl};

use super::{
    RegistryQuarantineFailure, RegistryQuarantineReason, RegistryQuarantineSource,
    RegistryRestoreHealth, RegistryTenantQuarantine, SpecRegistry, VerificationStatus,
};

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn minimal_csdl() -> (CsdlDocument, String) {
    (parse_csdl(CSDL_XML).unwrap(), CSDL_XML.to_string())
}

fn task_csdl() -> (CsdlDocument, String) {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Temper.Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="Task">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
          </EntityType>
          <EntityContainer Name="ExampleService">
            <EntitySet Name="Tasks" EntityType="Temper.Example.Task"/>
          </EntityContainer>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;
    (parse_csdl(xml).unwrap(), xml.to_string())
}

#[test]
fn register_and_lookup_tenant() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let tenant = TenantId::new("alpha");
    assert!(registry.get_tenant(&tenant).is_some());
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "NonExistent").is_none());
}

#[test]
fn unknown_tenant_returns_none() {
    let registry = SpecRegistry::new();
    let tenant = TenantId::new("unknown");
    assert!(registry.get_tenant(&tenant).is_none());
    assert!(registry.get_table(&tenant, "Order").is_none());
}

#[test]
fn multiple_tenants_are_isolated() {
    let mut registry = SpecRegistry::new();
    let (alpha_csdl, alpha_xml) = minimal_csdl();
    let (beta_csdl, beta_xml) = minimal_csdl();
    registry.register_tenant("alpha", alpha_csdl, alpha_xml, &[("Order", ORDER_IOA)]);
    registry.register_tenant("beta", beta_csdl, beta_xml, &[("Task", ORDER_IOA)]);
    let alpha = TenantId::new("alpha");
    let beta = TenantId::new("beta");
    assert!(registry.get_table(&alpha, "Order").is_some());
    assert!(registry.get_table(&alpha, "Task").is_none());
    assert!(registry.get_table(&beta, "Task").is_some());
    assert!(registry.get_table(&beta, "Order").is_none());
}

#[test]
fn deterministic_tenant_and_entity_lists() {
    let mut registry = SpecRegistry::new();
    let (alpha_csdl, alpha_xml) = minimal_csdl();
    let (beta_csdl, beta_xml) = minimal_csdl();
    registry.register_tenant("beta", beta_csdl, beta_xml, &[]);
    registry.register_tenant("alpha", alpha_csdl, alpha_xml, &[("Order", ORDER_IOA)]);
    assert_eq!(
        registry
            .tenant_ids()
            .into_iter()
            .map(TenantId::as_str)
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );
    assert_eq!(
        registry.entity_types(&TenantId::new("alpha")),
        vec!["Order"]
    );
}

#[test]
fn transition_table_is_functional() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let table = registry
        .get_table(&TenantId::new("alpha"), "Order")
        .unwrap();
    assert_eq!(table.entity_name, "Order");
    assert_eq!(table.initial_state, "Draft");
    assert!(table.evaluate("Draft", 1, "SubmitOrder").unwrap().success);
}

#[test]
fn removing_tenant_removes_its_specs() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("doomed", csdl, xml, &[("Order", ORDER_IOA)]);
    let tenant = TenantId::new("doomed");
    assert!(registry.remove_tenant(&tenant));
    assert!(registry.get_tenant(&tenant).is_none());
    assert!(!registry.remove_tenant(&tenant));
}

#[test]
fn spec_metadata_is_accessible() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let spec = registry.get_spec(&TenantId::new("alpha"), "Order").unwrap();
    assert_eq!(spec.automaton.automaton.name, "Order");
    assert!(!spec.ioa_source.is_empty());
}

#[test]
fn merge_preserves_existing_entities_and_entity_sets() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let (task, task_xml) = task_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            task,
            task_xml,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            true,
        )
        .unwrap();

    let tenant = TenantId::new("alpha");
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "Task").is_some());
    let config = registry.get_tenant(&tenant).unwrap();
    assert!(config.entity_set_map.contains_key("Orders"));
    assert!(config.entity_set_map.contains_key("Tasks"));
    assert!(matches!(
        config.verification.get("Task"),
        Some(VerificationStatus::Pending)
    ));
}

const CROSS_INVARIANTS: &str = r#"
version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "OrderStatusSanity"
kind = "hard"
on = "Order.*"
assert = 'related(Order, OrderId).status in ["Active"]'
"#;

#[test]
fn merge_without_cross_invariants_preserves_existing_ones() {
    let mut registry = registry_with_cross_invariants();
    let (task, task_xml) = task_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            task,
            task_xml,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            true,
        )
        .unwrap();
    assert_eq!(cross_invariant_count(&registry), 1);
}

#[test]
fn replace_without_cross_invariants_clears_existing_ones() {
    let mut registry = registry_with_cross_invariants();
    let (csdl, xml) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl,
            xml,
            &[("Order", ORDER_IOA)],
            Vec::new(),
            None,
            false,
        )
        .unwrap();
    assert_eq!(cross_invariant_count(&registry), 0);
}

#[test]
fn replace_removes_entities_missing_from_new_spec_set() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let (replacement, replacement_xml) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            replacement,
            replacement_xml,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            false,
        )
        .unwrap();
    let tenant = TenantId::new("alpha");
    assert!(registry.get_table(&tenant, "Order").is_none());
    assert!(registry.get_table(&tenant, "Task").is_some());
}

fn registry_with_cross_invariants() -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl,
            xml,
            &[("Order", ORDER_IOA)],
            Vec::new(),
            Some(CROSS_INVARIANTS.to_string()),
            false,
        )
        .unwrap();
    assert_eq!(cross_invariant_count(&registry), 1);
    registry
}

fn cross_invariant_count(registry: &SpecRegistry) -> usize {
    registry
        .get_tenant(&TenantId::new("alpha"))
        .and_then(|tenant| tenant.cross_invariants.as_ref())
        .map_or(0, |constraints| constraints.invariants.len())
}

#[test]
fn explicit_registration_cannot_clear_quarantine_before_durable_resolution() {
    let mut registry = SpecRegistry::new();
    registry.record_restore_health(&RegistryRestoreHealth {
        restored_specs: 0,
        quarantined_tenants: BTreeMap::from([(
            "alpha".to_string(),
            RegistryTenantQuarantine {
                entity_failures: BTreeMap::from([(
                    "Order".to_string(),
                    RegistryQuarantineFailure {
                        spec_version: 1,
                        constraint_version: None,
                        reason: RegistryQuarantineReason::InvalidCsdl,
                        source_kind: RegistryQuarantineSource::Csdl,
                        source_line: None,
                        source_column: None,
                        acknowledged: false,
                        detail: "invalid XML".to_string(),
                    },
                )]),
            },
        )]),
    });
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    assert!(
        registry.restore_health().is_quarantined("alpha", "Order"),
        "process health cannot outrun the durable compare-and-set"
    );

    registry.replace_tenant_restore_quarantine("alpha", None);
    assert!(registry.restore_health().is_healthy());
}

#[test]
fn tenant_quarantine_snapshot_replacement_removes_stale_entities() {
    let failure = |version| RegistryQuarantineFailure {
        spec_version: version,
        constraint_version: None,
        reason: RegistryQuarantineReason::InvalidCsdl,
        source_kind: RegistryQuarantineSource::Csdl,
        source_line: None,
        source_column: None,
        acknowledged: false,
        detail: "invalid XML".to_string(),
    };
    let mut registry = SpecRegistry::new();
    registry.record_restore_health(&RegistryRestoreHealth {
        restored_specs: 0,
        quarantined_tenants: BTreeMap::from([(
            "alpha".to_string(),
            RegistryTenantQuarantine {
                entity_failures: BTreeMap::from([
                    ("Order".to_string(), failure(1)),
                    ("Task".to_string(), failure(1)),
                ]),
            },
        )]),
    });

    registry.replace_tenant_restore_quarantine(
        "alpha",
        Some(RegistryTenantQuarantine {
            entity_failures: BTreeMap::from([("Order".to_string(), failure(2))]),
        }),
    );

    assert!(registry.restore_health().is_quarantined("alpha", "Order"));
    assert!(!registry.restore_health().is_quarantined("alpha", "Task"));
    assert_eq!(
        registry.restore_health().quarantined_tenants["alpha"].entity_failures["Order"]
            .spec_version,
        2
    );
}

#[test]
fn durable_acknowledgment_cas_preserves_constraint_removal_race() {
    let failure = |constraint_version, acknowledged| RegistryQuarantineFailure {
        spec_version: 1,
        constraint_version,
        reason: RegistryQuarantineReason::InvalidCsdl,
        source_kind: RegistryQuarantineSource::Csdl,
        source_line: None,
        source_column: None,
        acknowledged,
        detail: "invalid XML".to_string(),
    };
    let mut registry = SpecRegistry::new();
    registry.replace_tenant_restore_quarantine(
        "alpha",
        Some(RegistryTenantQuarantine {
            entity_failures: BTreeMap::from([("Order".to_string(), failure(Some(3), false))]),
        }),
    );
    let before = registry.restore_quarantine_identity("alpha", "Order");
    registry.replace_tenant_restore_quarantine(
        "alpha",
        Some(RegistryTenantQuarantine {
            entity_failures: BTreeMap::from([("Order".to_string(), failure(None, false))]),
        }),
    );

    assert!(!registry.reconcile_acknowledged_restore_quarantine(
        "alpha",
        "Order",
        before,
        failure(Some(3), true),
    ));
    let current = &registry.restore_health().quarantined_tenants["alpha"].entity_failures["Order"];
    assert_eq!(current.constraint_version, None);
    assert!(!current.acknowledged);
}
