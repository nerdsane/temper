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

/// A schema-level annotation (a direct child of `<Schema>`) is how an app
/// says what it is: `Temper.Twin` marks a twin schema and names it. It has
/// to survive parse and emit, or `$metadata` says nothing about it.
#[test]
fn emit_round_trips_schema_annotations() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Dsf.Twin" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <Annotation Term="Temper.Twin" String="Deep Sci-Fi"/>
          <Annotation Term="Temper.Graph.Roles">
            <Collection><String>host</String><String>model</String></Collection>
          </Annotation>
          <EntityType Name="Widget">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Annotation Term="Temper.Role" String="host"/>
          </EntityType>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let schema = &doc.schemas[0];
    assert_eq!(schema.annotations.len(), 2, "both schema annotations parse");
    assert_eq!(schema.annotations[0].term, "Temper.Twin");
    assert!(
        matches!(&schema.annotations[0].value, AnnotationValue::String(s) if s == "Deep Sci-Fi")
    );
    assert_eq!(
        schema.entity_types[0].annotations.len(),
        1,
        "entity annotations are unchanged"
    );

    let emitted = emit_csdl_xml(&doc);
    let doc2 = parse_csdl(&emitted).expect("emitted XML should re-parse");
    let schema2 = &doc2.schemas[0];
    assert_eq!(
        schema2.annotations.len(),
        2,
        "both schema annotations survive emit"
    );
    assert_eq!(schema2.annotations[0].term, "Temper.Twin");
    assert!(
        matches!(&schema2.annotations[0].value, AnnotationValue::String(s) if s == "Deep Sci-Fi")
    );
    assert!(
        matches!(&schema2.annotations[1].value, AnnotationValue::Collection(items) if items.len() == 2)
    );
    assert_eq!(schema2.entity_types[0].annotations[0].term, "Temper.Role");
}

/// `<Annotation …></Annotation>` reads like `<Annotation …/>` for every
/// inline value type, at schema and entity level alike.
#[test]
fn non_self_closing_annotations_keep_bool_and_int_values() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <Annotation Term="Temper.Enabled" Bool="true"></Annotation>
          <Annotation Term="Temper.Rank" Int="7"></Annotation>
          <EntityType Name="Widget">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Annotation Term="Temper.Enabled" Bool="false"></Annotation>
          </EntityType>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let schema = &doc.schemas[0];
    assert!(matches!(
        schema.annotations[0].value,
        AnnotationValue::Bool(true)
    ));
    assert!(matches!(
        schema.annotations[1].value,
        AnnotationValue::Int(7)
    ));
    assert!(matches!(
        schema.entity_types[0].annotations[0].value,
        AnnotationValue::Bool(false)
    ));

    let emitted = emit_csdl_xml(&doc);
    assert!(
        emitted.contains(r#"<Annotation Term="Temper.Enabled" Bool="true"/>"#),
        "{emitted}"
    );
    assert!(
        emitted.contains(r#"<Annotation Term="Temper.Rank" Int="7"/>"#),
        "{emitted}"
    );
}

/// A Record value round-trips through the `<Record><PropertyValue/></Record>`
/// shape the emitter writes, at schema and entity level.
#[test]
fn record_annotations_round_trip() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <Annotation Term="Temper.Owner">
            <Record>
              <PropertyValue Property="team" String="factory"/>
              <PropertyValue Property="contact" String="a &amp; b"/>
            </Record>
          </Annotation>
          <EntityType Name="Widget">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Annotation Term="Temper.Owner"><Record/></Annotation>
          </EntityType>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let schema = &doc.schemas[0];
    let AnnotationValue::Record(fields) = &schema.annotations[0].value else {
        panic!(
            "schema Record parses as Record, got {:?}",
            schema.annotations[0].value
        );
    };
    assert_eq!(fields.get("team").map(String::as_str), Some("factory"));
    assert!(
        matches!(&schema.entity_types[0].annotations[0].value, AnnotationValue::Record(f) if f.is_empty())
    );

    let emitted = emit_csdl_xml(&doc);
    let doc2 = parse_csdl(&emitted).expect("emitted XML should re-parse");
    let AnnotationValue::Record(fields2) = &doc2.schemas[0].annotations[0].value else {
        panic!("Record survives emit");
    };
    assert_eq!(fields2.len(), 2);
    assert_eq!(fields2.get("team").map(String::as_str), Some("factory"));
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

/// `<Annotations Target="Ns.Type/Property">` blocks are how a schema annotates
/// something from outside it — here, which entity types a property refers to,
/// the edges of a twin graph. They round-trip and keep their target.
#[test]
fn targeted_annotation_blocks_round_trip() {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Dsf.Twin" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="Service">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
            <Property Name="ApplicationId" Type="Edm.String"/>
          </EntityType>
          <Annotations Target="Dsf.Twin.Service/ApplicationId">
            <Annotation Term="Temper.References" String="Service,Project"/>
            <Annotation Term="Temper.ReferenceShape" String="single"/>
          </Annotations>
          <Annotations Target="Dsf.Twin.Service/Id" Qualifier="ui"><Annotation Term="Temper.Enabled" Bool="true"></Annotation></Annotations>
          <Annotations Target="Dsf.Twin.Service/Cleared"/>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;

    let doc = parse_csdl(xml).unwrap();
    let schema = &doc.schemas[0];
    assert_eq!(
        schema.targeted_annotations.len(),
        3,
        "all three blocks parse, the empty one included"
    );
    assert!(schema.targeted_annotations[2].annotations.is_empty());
    let refs = &schema.targeted_annotations[0];
    assert_eq!(refs.target, "Dsf.Twin.Service/ApplicationId");
    assert_eq!(refs.annotations.len(), 2);
    assert_eq!(refs.annotations[0].term, "Temper.References");
    assert!(
        matches!(&refs.annotations[0].value, AnnotationValue::String(s) if s == "Service,Project")
    );
    assert!(matches!(
        schema.targeted_annotations[1].annotations[0].value,
        AnnotationValue::Bool(true)
    ));
    assert_eq!(schema.entity_types.len(), 1, "the block is not an entity");

    let emitted = emit_csdl_xml(&doc);
    assert!(
        emitted.contains(r#"<Annotations Target="Dsf.Twin.Service/ApplicationId">"#),
        "{emitted}"
    );
    let doc2 = parse_csdl(&emitted).expect("emitted XML should re-parse");
    let again = &doc2.schemas[0].targeted_annotations;
    assert_eq!(again.len(), 3);
    assert_eq!(again[0].target, "Dsf.Twin.Service/ApplicationId");
    assert_eq!(again[0].annotations.len(), 2);
    assert_eq!(
        again[1].qualifier.as_deref(),
        Some("ui"),
        "the qualifier survives emit"
    );
    assert!(
        matches!(&again[0].annotations[0].value, AnnotationValue::String(s) if s == "Service,Project")
    );
}

#[test]
fn schema_alias_survives_parse_emit_and_serialization() {
    let xml = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices><Schema Namespace="Example.Types" Alias="ET" xmlns="http://docs.oasis-open.org/odata/ns/edm">
        <EntityType Name="Widget"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String"/></EntityType>
        <EntityContainer Name="Service"><EntitySet Name="Widgets" EntityType="ET.Widget"/></EntityContainer>
      </Schema></edmx:DataServices></edmx:Edmx>"#;
    let parsed = parse_csdl(xml).unwrap();
    assert_eq!(parsed.schemas[0].alias.as_deref(), Some("ET"));
    let round_trip = parse_csdl(&emit_csdl_xml(&parsed)).unwrap();
    assert_eq!(round_trip.schemas[0].alias.as_deref(), Some("ET"));
    assert_eq!(
        round_trip.schemas[0].entity_containers[0].entity_sets[0].entity_type,
        "ET.Widget"
    );
    let serialized = serde_json::to_string(&parsed).unwrap();
    let restored: CsdlDocument = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored.schemas[0].alias.as_deref(), Some("ET"));
    let mut legacy = serde_json::to_value(&parsed).unwrap();
    legacy["schemas"][0]
        .as_object_mut()
        .unwrap()
        .remove("alias");
    let restored: CsdlDocument = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.schemas[0].alias, None);
}

/// A CSDL whose `Child.Parent` relation declares `policy` as its delete policy.
fn csdl_with_delete_policy(policy: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="Parent">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.String" Nullable="false"/>
          </EntityType>
          <EntityType Name="Child">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.String" Nullable="false"/>
            <Property Name="parent_id" Type="Edm.String" Nullable="false"/>
            <NavigationProperty Name="Parent" Type="Test.Parent">
              <ReferentialConstraint Property="parent_id" ReferencedProperty="Id"/>
              <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="{policy}"/>
            </NavigationProperty>
          </EntityType>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#
    )
}

fn child_parent_policy(doc: &CsdlDocument) -> Option<crate::cross_invariant::DeletePolicy> {
    let child = doc.schemas[0].entity_type("Child").unwrap();
    child.navigation_properties[0].delete_policy
}

#[test]
fn navigation_delete_policy_survives_emit_and_reparse() {
    use crate::cross_invariant::DeletePolicy;
    for (name, policy) in [
        ("restrict", DeletePolicy::Restrict),
        ("cascade", DeletePolicy::Cascade),
        ("none", DeletePolicy::None),
    ] {
        let doc = parse_csdl(&csdl_with_delete_policy(name)).unwrap();
        assert_eq!(child_parent_policy(&doc), Some(policy));
        let reparsed = parse_csdl(&emit_csdl_xml(&doc)).expect("emitted XML should re-parse");
        assert_eq!(child_parent_policy(&reparsed), Some(policy), "{name}");
        let child = reparsed.schemas[0].entity_type("Child").unwrap();
        assert_eq!(
            child.navigation_properties[0].referential_constraints[0].property,
            "parent_id"
        );
    }
}

#[test]
fn navigation_without_delete_policy_declares_none() {
    let xml = csdl_with_delete_policy("cascade").replace(
        r#"<Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>"#,
        "",
    );
    let doc = parse_csdl(&xml).unwrap();
    assert_eq!(child_parent_policy(&doc), None);
    assert_eq!(
        child_parent_policy(&parse_csdl(&emit_csdl_xml(&doc)).unwrap()),
        None
    );
}

#[test]
fn unknown_or_unimplemented_delete_policy_is_refused() {
    for name in ["setnull", "Cascade", "orphan"] {
        let error = parse_csdl(&csdl_with_delete_policy(name))
            .expect_err("a delete policy the server cannot carry out must not parse");
        assert!(
            error.to_string().contains("Relation.DeletePolicy"),
            "{name}: {error}"
        );
    }
}

#[test]
fn delete_policy_as_a_string_element_reads_as_the_attribute_does() {
    use crate::cross_invariant::DeletePolicy;
    let attribute = r#"<Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>"#;
    let element = r#"<Annotation Term="Temper.Vocab.Relation.DeletePolicy"><String>cascade</String></Annotation>"#;
    let xml = csdl_with_delete_policy("cascade").replace(attribute, element);
    let doc = parse_csdl(&xml).expect("the String element form of a delete policy parses");
    assert_eq!(child_parent_policy(&doc), Some(DeletePolicy::Cascade));

    let collection = r#"<Annotation Term="Temper.Vocab.Relation.DeletePolicy"><Collection><String>cascade</String></Collection></Annotation>"#;
    let error = parse_csdl(&csdl_with_delete_policy("cascade").replace(attribute, collection))
        .expect_err("a Collection is not a delete policy, even one of one String");
    assert!(error.to_string().contains("must be a String"), "{error}");
}
