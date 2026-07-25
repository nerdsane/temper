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

#[tokio::test]
async fn postgres_restore_does_not_publish_uncommitted_staging() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("migrate Postgres");
    let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
    let tenant = format!("registry-staged-{}", uuid::Uuid::new_v4());
    let ioa_a = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let ioa_b = ioa_a.replace("#", "# staged restart\n#");
    let csdl_xml = csdl_xml_for("Order", "Orders");
    let fingerprint_a = temper_store_turso::spec_content_hash(ioa_a);
    let fingerprint_b = temper_store_turso::spec_content_hash(&ioa_b);

    store
        .upsert_spec(&tenant, "Order", ioa_a, &csdl_xml, &fingerprint_a)
        .await
        .expect("stage declaration A");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit declaration A");
    store
        .upsert_spec(&tenant, "Order", &ioa_b, &csdl_xml, &fingerprint_b)
        .await
        .expect("stage declaration B");

    let authority: (String, bool) = sqlx::query_as(
        "SELECT declaration_fingerprint, present \
         FROM spec_declaration_authority \
         WHERE tenant = $1 AND entity_type = 'Order'",
    )
    .bind(&tenant)
    .fetch_one(&pool)
    .await
    .expect("read committed authority");
    assert_eq!(authority, (fingerprint_a, true));

    let restored_rows = load_postgres_spec_rows(&pool)
        .await
        .expect("load committed registry rows");
    let restored = restored_rows
        .iter()
        .find(|row| row.tenant == tenant && row.entity_type == "Order")
        .expect("startup must retain committed A while B is staged");
    assert_eq!(restored.ioa_source, ioa_a);
}
