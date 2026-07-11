use std::collections::BTreeMap;

use super::*;

/// Mock implementation of SpecRowLike for testing row_to_registry_status.
struct MockRow {
    status: String,
    verified: bool,
    levels_passed: Option<i32>,
    levels_total: Option<i32>,
    updated_at: String,
    verification_result: Option<EntityVerificationResult>,
}

impl SpecRowLike for MockRow {
    fn spec_version(&self) -> i64 {
        1
    }
    fn verification_status(&self) -> &str {
        &self.status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        self.levels_passed
    }
    fn levels_total(&self) -> Option<i32> {
        self.levels_total
    }
    fn updated_at_rfc3339(&self) -> String {
        self.updated_at.clone()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result.clone()
    }
}

struct MockSpecRow {
    entity_type: String,
    ioa_source: String,
    csdl_xml: String,
    status: String,
    verified: bool,
}

impl SpecRowLike for MockSpecRow {
    fn spec_version(&self) -> i64 {
        7
    }
    fn verification_status(&self) -> &str {
        &self.status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        None
    }
    fn levels_total(&self) -> Option<i32> {
        None
    }
    fn updated_at_rfc3339(&self) -> String {
        "2026-04-25T00:00:00Z".to_string()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        None
    }
}

fn csdl_xml_for(entity_type: &str, set_name: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Temper.Restore" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="{entity_type}">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.String" Nullable="false"/>
          </EntityType>
          <EntityContainer Name="RestoreService">
            <EntitySet Name="{set_name}" EntityType="Temper.Restore.{entity_type}"/>
          </EntityContainer>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#
    )
}

#[test]
fn populate_registry_merges_csdl_fragments_from_all_restored_rows() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();
    let task_ioa = order_ioa.replace("name = \"Order\"", "name = \"Task\"");
    let rows = vec![
        MockSpecRow {
            entity_type: "Order".to_string(),
            ioa_source: order_ioa,
            csdl_xml: csdl_xml_for("Order", "Orders"),
            status: "passed".to_string(),
            verified: true,
        },
        MockSpecRow {
            entity_type: "Task".to_string(),
            ioa_source: task_ioa,
            csdl_xml: csdl_xml_for("Task", "Tasks"),
            status: "passed".to_string(),
            verified: true,
        },
    ];
    let mut grouped = BTreeMap::new();
    grouped.insert("default".to_string(), rows);
    let mut constraints = BTreeMap::new();
    let mut registry = SpecRegistry::new();

    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut constraints,
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );
    assert!(
        outcome.is_healthy(),
        "no tenant should be quarantined: {:?}",
        outcome.quarantined_tenants
    );
    assert_eq!(outcome.restored_specs, 2);

    let tenant = TenantId::new("default");
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "Task").is_some());
    assert_eq!(
        registry.resolve_entity_type(&tenant, "Orders").as_deref(),
        Some("Order")
    );
    assert_eq!(
        registry.resolve_entity_type(&tenant, "Tasks").as_deref(),
        Some("Task"),
        "restore must preserve every app's OData entity-set mapping"
    );
}

#[test]
fn populate_registry_isolates_corrupt_tenant() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();

    let healthy_rows = vec![MockSpecRow {
        entity_type: "Order".to_string(),
        ioa_source: order_ioa.clone(),
        csdl_xml: csdl_xml_for("Order", "Orders"),
        status: "passed".to_string(),
        verified: true,
    }];
    // Corrupt CSDL: malformed XML (EOF inside an open tag) → parse_csdl errors.
    let corrupt_rows = vec![MockSpecRow {
        entity_type: "Order".to_string(),
        ioa_source: order_ioa,
        csdl_xml: "<a><b".to_string(),
        status: "passed".to_string(),
        verified: true,
    }];

    // BTreeMap orders "corrupt" before "healthy", so the bad tenant is
    // processed first — proving it cannot abort the healthy one.
    let mut grouped = BTreeMap::new();
    grouped.insert("corrupt".to_string(), corrupt_rows);
    grouped.insert("healthy".to_string(), healthy_rows);
    let mut constraints = BTreeMap::new();
    let mut registry = SpecRegistry::new();

    // The production Postgres/Turso restore path. One corrupt tenant must NOT
    // abort the whole restore — it is logged, quarantined, and skipped.
    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut constraints,
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    // Healthy tenant still boots.
    let healthy = TenantId::new("healthy");
    assert!(
        registry.get_table(&healthy, "Order").is_some(),
        "healthy tenant must still boot after a corrupt tenant"
    );
    // Corrupt tenant is quarantined, not registered.
    let corrupt = TenantId::new("corrupt");
    assert!(
        registry.get_table(&corrupt, "Order").is_none(),
        "corrupt tenant must be quarantined"
    );

    // Exactly the healthy tenant's spec restored; the corrupt one is retained
    // under an explicit quarantine.
    assert_eq!(outcome.restored_specs, 1);
    assert_eq!(
        outcome
            .quarantined_tenants
            .get("corrupt")
            .and_then(|entry| entry.entity_failures.get("Order"))
            .map(|failure| failure.reason),
        Some(RegistryQuarantineReason::InvalidCsdl)
    );
    let failure = &outcome.quarantined_tenants["corrupt"].entity_failures["Order"];
    assert_eq!(failure.spec_version, 7);
    assert_eq!(failure.source_kind, RegistryQuarantineSource::Csdl);
    assert!(failure.detail.len() <= REGISTRY_QUARANTINE_DETAIL_BUDGET_BYTES);
    assert!(
        outcome.is_quarantined("corrupt", "Order"),
        "corrupt Order row must remain explicitly accounted for"
    );
}

#[test]
fn source_positions_require_a_marker_word_and_adjacent_number() {
    assert_eq!(
        source_position("no baseline value at offset 12; line 5, column: 7"),
        (Some(5), Some(7))
    );
    assert_eq!(source_position("online protocol 12; color 8"), (None, None));
    assert_eq!(source_position("lineage 3; 3-line buffer 4"), (None, None));
}

#[test]
fn failed_restore_does_not_partially_replace_existing_tenant() {
    let tenant = TenantId::new("default");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.clone(),
        parse_csdl(&csdl_xml_for("Order", "Orders")).unwrap(),
        csdl_xml_for("Order", "Orders"),
        &[("Order", order_ioa)],
    );
    let original_csdl = registry.get_tenant(&tenant).unwrap().csdl_xml.clone();

    let mut grouped = BTreeMap::new();
    grouped.insert(
        tenant.as_str().to_string(),
        vec![MockSpecRow {
            entity_type: "Task".to_string(),
            ioa_source: "[automaton]\nname = \"Task\"\n".to_string(),
            csdl_xml: csdl_xml_for("Task", "Tasks"),
            status: "passed".to_string(),
            verified: true,
        }],
    );
    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    assert!(outcome.is_quarantined("default", "Task"));
    assert_eq!(
        registry
            .get_tenant(&tenant)
            .map(|config| config.csdl_xml.as_str()),
        Some(original_csdl.as_str()),
        "failed re-registration must preserve the last-known-good CSDL"
    );
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "Task").is_none());
    assert_eq!(
        registry.resolve_entity_type(&tenant, "Orders").as_deref(),
        Some("Order")
    );
}

#[test]
fn registration_quarantine_attributes_only_the_failing_ioa_as_source() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();
    let mut grouped = BTreeMap::new();
    grouped.insert(
        "default".to_string(),
        vec![
            MockSpecRow {
                entity_type: "Order".to_string(),
                ioa_source: order_ioa,
                csdl_xml: csdl_xml_for("Order", "Orders"),
                status: "passed".to_string(),
                verified: true,
            },
            MockSpecRow {
                entity_type: "Task".to_string(),
                ioa_source: "[automaton]\nname = \"Task\"\n".to_string(),
                csdl_xml: csdl_xml_for("Task", "Tasks"),
                status: "passed".to_string(),
                verified: true,
            },
        ],
    );
    let mut registry = SpecRegistry::new();
    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );
    let failures = &outcome.quarantined_tenants["default"].entity_failures;
    assert_eq!(failures["Task"].source_kind, RegistryQuarantineSource::Ioa);
    assert_eq!(
        failures["Order"].source_kind,
        RegistryQuarantineSource::Registration
    );
    assert!(failures["Order"].detail.contains("sibling entity 'Task'"));
}

#[test]
fn invalid_csdl_quarantine_attributes_only_the_failing_fragment_as_source() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();
    let task_ioa = order_ioa.replace("name = \"Order\"", "name = \"Task\"");
    let mut grouped = BTreeMap::new();
    grouped.insert(
        "default".to_string(),
        vec![
            MockSpecRow {
                entity_type: "Order".to_string(),
                ioa_source: order_ioa,
                csdl_xml: csdl_xml_for("Order", "Orders"),
                status: "passed".to_string(),
                verified: true,
            },
            MockSpecRow {
                entity_type: "Task".to_string(),
                ioa_source: task_ioa,
                csdl_xml: "<a><b".to_string(),
                status: "passed".to_string(),
                verified: true,
            },
        ],
    );
    let mut registry = SpecRegistry::new();
    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );
    let failures = &outcome.quarantined_tenants["default"].entity_failures;
    assert_eq!(failures["Task"].source_kind, RegistryQuarantineSource::Csdl);
    assert_eq!(
        failures["Order"].source_kind,
        RegistryQuarantineSource::Registration
    );
    assert!(failures["Order"].detail.contains("sibling CSDL fragment"));
}

#[test]
fn row_to_registry_status_pending() {
    let status = row_to_registry_status(&MockRow {
        status: "pending".into(),
        verified: false,
        levels_passed: None,
        levels_total: None,
        updated_at: "2024-01-01T00:00:00Z".into(),
        verification_result: None,
    });
    assert!(matches!(status, VerificationStatus::Pending));
}

#[test]
fn row_to_registry_status_running() {
    let status = row_to_registry_status(&MockRow {
        status: "running".into(),
        verified: false,
        levels_passed: None,
        levels_total: None,
        updated_at: "2024-01-01T00:00:00Z".into(),
        verification_result: None,
    });
    assert!(matches!(status, VerificationStatus::Running));
}

#[test]
fn row_to_registry_status_passed() {
    let status = row_to_registry_status(&MockRow {
        status: "passed".into(),
        verified: true,
        levels_passed: Some(3),
        levels_total: Some(3),
        updated_at: "2024-01-01T00:00:00Z".into(),
        verification_result: None,
    });
    match status {
        VerificationStatus::Restored(result) => assert!(result.all_passed),
        other => panic!("Expected Restored, got {other:?}"),
    }
}

#[test]
fn row_to_registry_status_failed() {
    let status = row_to_registry_status(&MockRow {
        status: "failed".into(),
        verified: false,
        levels_passed: Some(1),
        levels_total: Some(3),
        updated_at: "2024-01-01T00:00:00Z".into(),
        verification_result: None,
    });
    match status {
        VerificationStatus::Restored(result) => {
            assert!(!result.all_passed);
            assert_eq!(result.levels.len(), 3);
            assert!(result.levels[0].passed);
            assert!(!result.levels[1].passed);
        }
        other => panic!("Expected Restored, got {other:?}"),
    }
}
