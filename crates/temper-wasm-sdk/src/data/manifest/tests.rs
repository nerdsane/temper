use super::*;

#[test]
fn canonical_grant_digest_ignores_input_set_order() {
    let mut first = ModuleDataGrant::default();
    first.operations.insert(DataOperationKind::EntityPatch);
    first.operations.insert(DataOperationKind::EntityGet);
    let mut second = ModuleDataGrant::default();
    second.operations.insert(DataOperationKind::EntityGet);
    second.operations.insert(DataOperationKind::EntityPatch);
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
}

#[test]
fn canonical_grant_digest_ignores_entity_declaration_order() {
    let entity = |name: &str| EntityDataGrant {
        entity_type: name.into(),
        ..EntityDataGrant::default()
    };
    let first = ModuleDataGrant {
        entities: vec![entity("Temper.B"), entity("Temper.A")],
        ..ModuleDataGrant::default()
    };
    let second = ModuleDataGrant {
        entities: vec![entity("Temper.A"), entity("Temper.B")],
        ..ModuleDataGrant::default()
    };
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
}

#[test]
fn sequence_order_grant_is_wire_absent_until_enabled() {
    let mut entity = EntityDataGrant {
        entity_type: "Temper.Task".into(),
        ..EntityDataGrant::default()
    };
    let disabled = serde_json::to_value(&entity).unwrap();
    assert!(disabled.get("query_order_by_sequence").is_none());

    entity.query_order_by_sequence = true;
    let enabled = serde_json::to_value(&entity).unwrap();
    assert_eq!(enabled["query_order_by_sequence"], serde_json::json!(true));
}

#[test]
fn duplicate_entity_grants_fail() {
    let grant = ModuleDataGrant {
        entities: vec![
            EntityDataGrant {
                entity_type: "Temper.Task".into(),
                ..EntityDataGrant::default()
            },
            EntityDataGrant {
                entity_type: "Temper.Task".into(),
                ..EntityDataGrant::default()
            },
        ],
        ..ModuleDataGrant::default()
    };
    assert!(grant.validate().is_err());
}

#[test]
fn missing_operation_denies_even_when_entity_exists() {
    let grant = ModuleDataGrant {
        entities: vec![EntityDataGrant {
            entity_type: "Temper.Task".into(),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    };
    assert!(!grant.permits(DataOperationKind::EntityGet, "Temper.Task", None));
}

#[test]
fn semantic_hashes_change_when_a_used_property_changes() {
    let entity = |properties| ManifestEntityV1 {
        entity_type: "Temper.Task".into(),
        entity_set: "Tasks".into(),
        generated_name: "Task".into(),
        properties,
        actions: Vec::new(),
    };
    let property = |name: &str, nullable| ManifestPropertyV1 {
        canonical_name: name.into(),
        generated_name: name.to_lowercase(),
        type_name: "Edm.String".into(),
        nullable,
        enum_members: Vec::new(),
    };
    let manifest = |properties| {
        ModuleSdkManifest::new(
            "worker",
            ModuleSdkMetadataDigests {
                closure: "closure".into(),
                dependency_lock: "closure".into(),
                schema: "schema".into(),
            },
            "artifact",
            ModuleDataGrant::default(),
            vec![entity(properties)],
            BTreeSet::new(),
        )
        .unwrap()
    };
    let old = manifest(vec![property("Id", false)]);
    let changed = manifest(vec![property("Id", true)]);
    assert_ne!(
        old.used_symbol_hashes().unwrap(),
        changed.used_symbol_hashes().unwrap()
    );
}
