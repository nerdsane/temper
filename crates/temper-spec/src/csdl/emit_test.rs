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

/// Every string-valued attribute must survive adversarial markup without
/// creating schema structure or changing the typed document.
#[test]
fn adversarial_identifiers_do_not_inject_markup() {
    let hostile = |label: &str| format!("{label}\"><Injected Property=\"&<>'");
    let doc = CsdlDocument {
        version: hostile("4.0"),
        schemas: vec![Schema {
            namespace: hostile("Namespace"),
            entity_types: vec![EntityType {
                name: "Widget\" HasStream=\"true\"><Injected Property=\"&<>'".to_string(),
                key_properties: vec![hostile("Key")],
                properties: vec![Property {
                    name: "Name\"/><Property Name=\"Smuggled".to_string(),
                    type_name: hostile("PropertyType"),
                    nullable: true,
                    default_value: Some(hostile("Default")),
                    precision: None,
                    scale: None,
                }],
                navigation_properties: vec![NavigationProperty {
                    name: hostile("Navigation"),
                    type_name: hostile("NavigationType"),
                    nullable: true,
                    contains_target: false,
                    referential_constraints: vec![ReferentialConstraint {
                        property: hostile("ConstraintProperty"),
                        referenced_property: hostile("ConstraintTarget"),
                    }],
                }],
                annotations: vec![Annotation {
                    term: hostile("EntityAnnotationTerm"),
                    value: AnnotationValue::String(hostile("EntityAnnotationValue")),
                }],
                has_stream: false,
            }],
            enum_types: vec![EnumType {
                name: hostile("Enum"),
                members: vec![EnumMember {
                    name: hostile("Member"),
                    value: Some(7),
                }],
            }],
            actions: vec![Action {
                name: hostile("Action"),
                is_bound: true,
                parameters: vec![Parameter {
                    name: hostile("ActionParameter"),
                    type_name: hostile("ActionParameterType"),
                    nullable: true,
                    default_value: Some(hostile("ActionDefault")),
                }],
                return_type: Some(ReturnType {
                    type_name: hostile("ActionReturnType"),
                    nullable: true,
                    precision: None,
                    scale: None,
                }),
                annotations: vec![Annotation {
                    term: hostile("ActionAnnotationTerm"),
                    value: AnnotationValue::String(hostile("ActionAnnotationValue")),
                }],
            }],
            functions: vec![Function {
                name: hostile("Function"),
                is_bound: true,
                parameters: vec![Parameter {
                    name: hostile("FunctionParameter"),
                    type_name: hostile("FunctionParameterType"),
                    nullable: true,
                    default_value: Some(hostile("FunctionDefault")),
                }],
                return_type: Some(ReturnType {
                    type_name: hostile("FunctionReturnType"),
                    nullable: true,
                    precision: None,
                    scale: None,
                }),
                annotations: vec![Annotation {
                    term: hostile("FunctionAnnotationTerm"),
                    value: AnnotationValue::String(hostile("FunctionAnnotationValue")),
                }],
            }],
            entity_containers: vec![EntityContainer {
                name: hostile("Container"),
                entity_sets: vec![EntitySet {
                    name: hostile("EntitySet"),
                    entity_type: hostile("EntitySetType"),
                    navigation_bindings: vec![NavigationBinding {
                        path: hostile("BindingPath"),
                        target: hostile("BindingTarget"),
                    }],
                }],
                action_imports: vec![ActionImport {
                    name: hostile("ActionImport"),
                    action: hostile("ActionReference"),
                }],
                function_imports: vec![FunctionImport {
                    name: hostile("FunctionImport"),
                    function: hostile("FunctionReference"),
                }],
            }],
            terms: vec![Term {
                name: hostile("Term"),
                type_name: hostile("TermType"),
                applies_to: Some(hostile("AppliesTo")),
                description: Some(hostile("Description")),
            }],
        }],
    };

    let emitted = emit_csdl_xml(&doc);
    // The adversarial substrings may legitimately appear *escaped* inside an
    // attribute value; what must never appear is live markup.
    assert!(
        !emitted.contains("<Injected/>"),
        "attribute value injected live markup:\n{emitted}"
    );
    assert!(
        !emitted.contains("<Property Name=\"Smuggled"),
        "property name injected a sibling element:\n{emitted}"
    );

    let reparsed = parse_csdl(&emitted).expect("adversarial emit must stay well-formed");
    assert_eq!(
        serde_json::to_value(reparsed).expect("reparsed document should serialize"),
        serde_json::to_value(doc).expect("original document should serialize"),
        "adversarial attributes changed the typed CSDL document"
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

/// Whitespace and markup-significant characters in collection text nodes
/// survive without becoming literal character-reference strings.
#[test]
fn whitespace_in_collection_text_round_trips() {
    let value = "line one\nline\ttwo<&>".to_string();
    let doc = CsdlDocument {
        version: "4.0".to_string(),
        schemas: vec![Schema {
            namespace: "Test".to_string(),
            entity_types: vec![EntityType {
                name: "Widget".to_string(),
                key_properties: Vec::new(),
                properties: Vec::new(),
                navigation_properties: Vec::new(),
                annotations: vec![Annotation {
                    term: "Test.Values".to_string(),
                    value: AnnotationValue::Collection(vec![value.clone()]),
                }],
                has_stream: false,
            }],
            enum_types: Vec::new(),
            actions: Vec::new(),
            functions: Vec::new(),
            entity_containers: Vec::new(),
            terms: Vec::new(),
        }],
    };

    let emitted = emit_csdl_xml(&doc);
    assert!(emitted.contains("line one\nline\ttwo&lt;&amp;&gt;"));

    let reparsed = parse_csdl(&emitted).expect("emitted XML should re-parse");
    let AnnotationValue::Collection(items) =
        &reparsed.schemas[0].entity_types[0].annotations[0].value
    else {
        panic!("annotation should remain a collection");
    };
    assert_eq!(items, &[value]);
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
