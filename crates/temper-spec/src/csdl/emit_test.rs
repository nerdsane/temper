use super::*;
use crate::csdl::parse_csdl;

#[test]
fn emit_round_trips_minimal_csdl() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="Widget">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Property Name="Name" Type="Edm.String"/>
          </EntityType>
          <EntityContainer Name="Svc">
            <EntitySet Name="Widgets" EntityType="Test.Widget"/>
          </EntityContainer>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let emitted = emit_csdl_xml(&doc);

    // Parse the emitted XML back and verify structure is preserved.
    let doc2 = parse_csdl(&emitted).expect("emitted XML should re-parse");
    assert_eq!(doc2.version, "4.0");
    assert_eq!(doc2.schemas.len(), 1);
    let schema = &doc2.schemas[0];
    assert_eq!(schema.namespace, "Test");
    assert_eq!(schema.entity_types.len(), 1);
    assert_eq!(schema.entity_types[0].name, "Widget");
    assert_eq!(schema.entity_types[0].key_properties, vec!["Id"]);
    assert_eq!(schema.entity_types[0].properties.len(), 2);
    assert_eq!(schema.entity_containers.len(), 1);
    assert_eq!(schema.entity_containers[0].entity_sets.len(), 1);
    assert_eq!(
        schema.entity_containers[0].entity_sets[0].entity_type,
        "Test.Widget"
    );
}

#[test]
fn emit_round_trips_has_stream() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="MediaFile" HasStream="true">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Property Name="Name" Type="Edm.String"/>
          </EntityType>
          <EntityType Name="RegularEntity">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
          </EntityType>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let schema = &doc.schemas[0];

    let media = schema.entity_type("MediaFile").unwrap();
    assert!(media.has_stream, "MediaFile should have has_stream=true");

    let regular = schema.entity_type("RegularEntity").unwrap();
    assert!(
        !regular.has_stream,
        "RegularEntity should have has_stream=false"
    );

    // Round-trip
    let emitted = emit_csdl_xml(&doc);
    let doc2 = parse_csdl(&emitted).unwrap();
    let schema2 = &doc2.schemas[0];

    assert!(schema2.entity_type("MediaFile").unwrap().has_stream);
    assert!(!schema2.entity_type("RegularEntity").unwrap().has_stream);
}

/// A property name carrying a quote must not be able to close the attribute
/// and inject markup of its own.
#[test]
fn adversarial_identifiers_do_not_inject_markup() {
    let doc = CsdlDocument {
        version: "4.0".to_string(),
        schemas: vec![Schema {
            namespace: "Ns\"><Injected/><Schema Namespace=\"Evil".to_string(),
            entity_types: vec![EntityType {
                name: "Widget\" HasStream=\"true".to_string(),
                key_properties: vec!["Id<&>".to_string()],
                properties: vec![Property {
                    name: "Name\"/><Property Name=\"Smuggled".to_string(),
                    type_name: "Edm.String".to_string(),
                    nullable: true,
                    default_value: Some("a\"b&c<d".to_string()),
                    precision: None,
                    scale: None,
                }],
                navigation_properties: Vec::new(),
                annotations: vec![Annotation {
                    term: "Ns.Term\"><Injected/><Annotation Term=\"Evil".to_string(),
                    value: AnnotationValue::String("v\"><Injected/>".to_string()),
                }],
                has_stream: false,
            }],
            enum_types: Vec::new(),
            actions: Vec::new(),
            functions: Vec::new(),
            entity_containers: vec![EntityContainer {
                name: "Svc\"><Injected/><EntityContainer Name=\"Evil".to_string(),
                entity_sets: vec![EntitySet {
                    name: "Widgets\"><Injected/>".to_string(),
                    entity_type: "Test.Widget\"><Injected/>".to_string(),
                    navigation_bindings: vec![NavigationBinding {
                        path: "Path\"><Injected/>".to_string(),
                        target: "Target\"><Injected/>".to_string(),
                    }],
                }],
                action_imports: vec![ActionImport {
                    name: "DoIt\"><Injected/>".to_string(),
                    action: "Test.DoIt\"><Injected/>".to_string(),
                }],
                function_imports: vec![FunctionImport {
                    name: "GetIt\"><Injected/>".to_string(),
                    function: "Test.GetIt\"><Injected/>".to_string(),
                }],
            }],
            terms: Vec::new(),
        }],
    };

    let emitted = emit_csdl_xml(&doc);
    // The adversarial substrings may legitimately appear *escaped* inside an
    // attribute value; what must never appear is live markup.
    assert!(
        !emitted.contains("<Injected/>"),
        "namespace escaped its attribute:\n{emitted}"
    );
    assert!(
        !emitted.contains("<Property Name=\"Smuggled"),
        "property name injected a sibling element:\n{emitted}"
    );

    let reparsed = parse_csdl(&emitted).expect("adversarial emit must stay well-formed");
    let schema = &reparsed.schemas[0];
    assert_eq!(schema.namespace, doc.schemas[0].namespace);
    assert_eq!(
        schema.entity_types.len(),
        1,
        "injection created extra entity types"
    );

    let entity_type = &schema.entity_types[0];
    let original = &doc.schemas[0].entity_types[0];
    assert_eq!(entity_type.name, original.name);
    assert!(
        !entity_type.has_stream,
        "smuggled HasStream=\"true\" changed the typed model"
    );
    assert_eq!(entity_type.key_properties, original.key_properties);
    assert_eq!(entity_type.properties.len(), 1);
    assert_eq!(entity_type.properties[0].name, original.properties[0].name);
    assert_eq!(
        entity_type.properties[0].default_value,
        original.properties[0].default_value
    );

    assert_eq!(entity_type.annotations.len(), 1);
    assert_eq!(
        entity_type.annotations[0].term,
        original.annotations[0].term
    );

    // Container-side identifiers were unescaped before this fix too, so pin
    // them against regression rather than trusting the emitter by symmetry.
    assert_eq!(
        schema.entity_containers.len(),
        1,
        "injection created extra entity containers"
    );
    let container = &schema.entity_containers[0];
    let original_container = &doc.schemas[0].entity_containers[0];
    assert_eq!(container.name, original_container.name);

    assert_eq!(container.entity_sets.len(), 1);
    let entity_set = &container.entity_sets[0];
    let original_set = &original_container.entity_sets[0];
    assert_eq!(entity_set.name, original_set.name);
    assert_eq!(entity_set.entity_type, original_set.entity_type);

    assert_eq!(entity_set.navigation_bindings.len(), 1);
    assert_eq!(
        entity_set.navigation_bindings[0].path,
        original_set.navigation_bindings[0].path
    );
    assert_eq!(
        entity_set.navigation_bindings[0].target,
        original_set.navigation_bindings[0].target
    );

    assert_eq!(container.action_imports.len(), 1);
    assert_eq!(
        container.action_imports[0].action,
        original_container.action_imports[0].action
    );
    assert_eq!(container.function_imports.len(), 1);
    assert_eq!(
        container.function_imports[0].function,
        original_container.function_imports[0].function
    );
}

/// Whitespace inside attribute values survives the round trip rather than
/// being collapsed by attribute-value normalization.
#[test]
fn whitespace_in_attribute_values_round_trips() {
    let doc = CsdlDocument {
        version: "4.0".to_string(),
        schemas: vec![Schema {
            namespace: "Test".to_string(),
            entity_types: Vec::new(),
            enum_types: Vec::new(),
            actions: Vec::new(),
            functions: Vec::new(),
            entity_containers: Vec::new(),
            terms: vec![Term {
                name: "Note".to_string(),
                type_name: "Edm.String".to_string(),
                applies_to: None,
                description: Some("line one\nline\ttwo\rend".to_string()),
            }],
        }],
    };

    let reparsed = parse_csdl(&emit_csdl_xml(&doc)).expect("emitted XML should re-parse");
    assert_eq!(
        reparsed.schemas[0].terms[0].description,
        doc.schemas[0].terms[0].description
    );
}

#[test]
fn emit_round_trips_reference_csdl() {
    let xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let doc = parse_csdl(xml).unwrap();
    let emitted = emit_csdl_xml(&doc);

    let doc2 = parse_csdl(&emitted).expect("emitted reference CSDL should re-parse");
    assert_eq!(doc2.schemas.len(), doc.schemas.len());

    // Verify entity types are preserved.
    for (s1, s2) in doc.schemas.iter().zip(doc2.schemas.iter()) {
        assert_eq!(s1.namespace, s2.namespace);
        assert_eq!(s1.entity_types.len(), s2.entity_types.len());
        assert_eq!(s1.actions.len(), s2.actions.len());
        assert_eq!(s1.entity_containers.len(), s2.entity_containers.len());
    }
}
