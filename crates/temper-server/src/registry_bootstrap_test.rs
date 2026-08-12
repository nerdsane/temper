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

    populate_registry(
        &mut registry,
        grouped,
        &mut constraints,
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
    .expect("restore should register tenant");

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
fn populate_registry_rejects_restored_unsupported_safety_invariant() {
    let unsupported = format!(
        r#"{}

[[invariant]]
name = "UnsupportedRestoredSafety"
when = ["Draft"]
assert = "ghost ** quota"
"#,
        include_str!("../../../test-fixtures/specs/order.ioa.toml")
    );
    let rows = vec![MockSpecRow {
        entity_type: "Order".to_string(),
        ioa_source: unsupported,
        csdl_xml: csdl_xml_for("Order", "Orders"),
        status: "passed".to_string(),
        verified: true,
    }];
    let mut grouped = BTreeMap::new();
    grouped.insert("default".to_string(), rows);
    let mut registry = SpecRegistry::new();

    let error = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
    .expect_err("unsupported restored safety must not activate");

    assert!(error.contains("UnsupportedRestoredSafety"));
    assert!(registry.get_tenant(&TenantId::default()).is_none());
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
