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
    );

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

/// A spec stored before the predicate grammar: an old-syntax guard table.
const LEGACY_TASK_IOA: &str = r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"
guard = [{ type = "is_true", var = "ready" }]
"#;

#[test]
fn populate_registry_converts_a_stored_spec_in_the_old_syntax() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();
    let legacy_ioa = LEGACY_TASK_IOA.to_string();
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
            ioa_source: legacy_ioa,
            csdl_xml: csdl_xml_for("Task", "Tasks"),
            status: "passed".to_string(),
            verified: true,
        },
    ];
    let mut grouped = BTreeMap::new();
    grouped.insert("default".to_string(), rows);
    let mut registry = SpecRegistry::new();

    let restored = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    let tenant = TenantId::new("default");
    assert_eq!(restored, 2);
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(
        registry.get_table(&tenant, "Task").is_some(),
        "a stored spec in the old syntax is converted, not dropped"
    );
}

/// A stored spec that neither parses nor converts.
const UNREADABLE_IOA: &str = "[automaton]\nname = \"Broken\"\nstates = [\n";

#[test]
fn populate_registry_skips_a_stored_spec_that_cannot_be_read() {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string();
    let rows = vec![
        MockSpecRow {
            entity_type: "Order".to_string(),
            ioa_source: order_ioa,
            csdl_xml: csdl_xml_for("Order", "Orders"),
            status: "passed".to_string(),
            verified: true,
        },
        MockSpecRow {
            entity_type: "Broken".to_string(),
            ioa_source: UNREADABLE_IOA.to_string(),
            csdl_xml: csdl_xml_for("Broken", "Brokens"),
            status: "passed".to_string(),
            verified: true,
        },
    ];
    let mut grouped = BTreeMap::new();
    grouped.insert("default".to_string(), rows);
    let mut registry = SpecRegistry::new();

    let restored = populate_registry(
        &mut registry,
        grouped,
        &mut BTreeMap::new(),
        |row| Some(row.csdl_xml.clone()),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    let tenant = TenantId::new("default");
    assert_eq!(restored, 1);
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "Broken").is_none());
}

/// The platform-store restore TemperPaw runs at boot: an old-syntax spec is
/// converted, registered and written back committed, so the next boot reads
/// it as stored; an unreadable spec stays in the store and only its own type
/// is unavailable; the rest of the tenant restores.
#[tokio::test]
async fn platform_store_restore_converts_old_syntax_and_keeps_unreadable_specs() {
    use crate::platform_store::{PlatformStore, SimPlatformStore};

    let store = SimPlatformStore::no_faults(7);
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let rows = [
        (
            "Order",
            order_ioa.to_string(),
            csdl_xml_for("Order", "Orders"),
        ),
        (
            "Task",
            LEGACY_TASK_IOA.to_string(),
            csdl_xml_for("Task", "Tasks"),
        ),
        (
            "Broken",
            UNREADABLE_IOA.to_string(),
            csdl_xml_for("Broken", "Brokens"),
        ),
    ];
    for (entity_type, ioa, csdl) in &rows {
        let hash = temper_store_turso::spec_content_hash(ioa);
        store
            .upsert_spec("default", entity_type, ioa, csdl, &hash)
            .await
            .expect("seed spec");
    }
    store
        .commit_specs("default")
        .await
        .expect("commit seed specs");

    let mut registry = SpecRegistry::new();
    let restored = restore_registry_from_platform_store(&mut registry, &store)
        .await
        .expect("restore");

    let tenant = TenantId::new("default");
    assert_eq!(restored, 2);
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "Task").is_some());
    assert!(registry.get_table(&tenant, "Broken").is_none());

    let stored: BTreeMap<String, crate::platform_store::SpecRow> = store
        .load_specs()
        .await
        .expect("load specs")
        .into_iter()
        .map(|row| (row.entity_type.clone(), row))
        .collect();
    let task = stored
        .get("Task")
        .expect("converted spec is still stored and committed");
    assert!(
        temper_spec::automaton::parse_automaton(&task.ioa_source).is_ok(),
        "the stored Task spec is now in the current syntax"
    );
    assert_eq!(
        task.content_hash,
        temper_store_turso::spec_content_hash(&task.ioa_source)
    );
    assert_eq!(
        stored.get("Broken").map(|row| row.ioa_source.as_str()),
        Some(UNREADABLE_IOA),
        "an unreadable spec is kept as stored, not deleted"
    );
    assert!(stored.contains_key("Order"));
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

/// Converting a stored spec rewrites its row, which resets verification; a spec
/// that was verified as stored must stay verified as converted, or its entity
/// type refuses writes after the upgrade. One that was not verified stays so.
#[tokio::test]
async fn converted_specs_keep_their_verification() {
    use crate::platform_store::{PlatformStore, SpecVerificationUpdate};

    let directory = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", directory.path().join("specs.db").display());
    let store = temper_store_turso::TursoEventStore::new(&url, None)
        .await
        .expect("turso store");
    let unverified_ioa = LEGACY_TASK_IOA.replace("name = \"Task\"", "name = \"Draft\"");
    for (entity_type, ioa, set) in [
        ("Task", LEGACY_TASK_IOA, "Tasks"),
        ("Draft", unverified_ioa.as_str(), "Drafts"),
    ] {
        let hash = temper_store_turso::spec_content_hash(ioa);
        PlatformStore::upsert_spec(
            &store,
            "default",
            entity_type,
            ioa,
            &csdl_xml_for(entity_type, set),
            &hash,
        )
        .await
        .expect("seed spec");
    }
    PlatformStore::commit_specs(&store, "default")
        .await
        .expect("commit");
    PlatformStore::persist_spec_verification(
        &store,
        "default",
        "Task",
        SpecVerificationUpdate {
            status: "completed",
            verified: true,
            levels_passed: None,
            levels_total: None,
            verification_result_json: None,
        },
    )
    .await
    .expect("verify Task");

    let mut registry = SpecRegistry::new();
    let restored = restore_registry_from_platform_store(&mut registry, &store)
        .await
        .expect("restore");
    assert_eq!(restored, 2);

    let rows: BTreeMap<String, crate::platform_store::SpecRow> = PlatformStore::load_specs(&store)
        .await
        .expect("load specs")
        .into_iter()
        .map(|row| (row.entity_type.clone(), row))
        .collect();
    let cache = PlatformStore::load_verification_cache(&store, "default")
        .await
        .expect("verification cache");
    let task = rows.get("Task").expect("Task stored");
    assert!(temper_spec::automaton::parse_automaton(&task.ioa_source).is_ok());
    assert_eq!(
        cache.get("Task"),
        Some(&(task.content_hash.clone(), true)),
        "a verified spec stays verified under its converted source"
    );
    assert_eq!(
        cache.get("Draft").map(|(_, verified)| *verified),
        Some(false),
        "an unverified spec is not marked verified by the conversion"
    );
}

/// Converting stored specs commits only the converted rows: another install
/// in the tenant that is still pending stays uncommitted.
#[tokio::test]
async fn converted_specs_commit_only_their_own_rows() {
    use crate::platform_store::PlatformStore;

    let directory = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", directory.path().join("specs.db").display());
    let store = temper_store_turso::TursoEventStore::new(&url, None)
        .await
        .expect("turso store");
    let task_csdl = csdl_xml_for("Task", "Tasks");
    PlatformStore::upsert_spec(
        &store,
        "default",
        "Task",
        LEGACY_TASK_IOA,
        &task_csdl,
        &temper_store_turso::spec_content_hash(LEGACY_TASK_IOA),
    )
    .await
    .expect("seed Task");
    PlatformStore::commit_specs(&store, "default")
        .await
        .expect("commit Task");
    let pending_ioa = "[automaton]\nname = \"Note\"\nstates = [\"Draft\"]\ninitial = \"Draft\"\n";
    PlatformStore::upsert_spec(
        &store,
        "default",
        "Note",
        pending_ioa,
        &csdl_xml_for("Note", "Notes"),
        &temper_store_turso::spec_content_hash(pending_ioa),
    )
    .await
    .expect("seed a pending install");

    let mut registry = SpecRegistry::new();
    restore_registry_from_platform_store(&mut registry, &store)
        .await
        .expect("restore");

    let restorable: Vec<String> = PlatformStore::load_specs(&store)
        .await
        .expect("load specs")
        .into_iter()
        .map(|row| row.entity_type)
        .collect();
    assert_eq!(
        restorable,
        vec!["Task".to_string()],
        "the pending Note install must not be committed by the conversion"
    );
}
