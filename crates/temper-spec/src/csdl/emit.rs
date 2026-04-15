//! Serialize a [`CsdlDocument`] back to OData CSDL XML.

use super::types::*;

/// Serialize a [`CsdlDocument`] to an OData 4.0 CSDL XML string.
///
/// The output is a valid `edmx:Edmx` document that can be served as `$metadata`
/// or round-tripped through [`parse_csdl`](super::parse_csdl).
pub fn emit_csdl_xml(doc: &CsdlDocument) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!(
        "<edmx:Edmx Version=\"{}\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\">\n",
        doc.version
    ));
    out.push_str("  <edmx:DataServices>\n");

    for schema in &doc.schemas {
        emit_schema(&mut out, schema);
    }

    out.push_str("  </edmx:DataServices>\n");
    out.push_str("</edmx:Edmx>\n");
    out
}

fn emit_schema(out: &mut String, schema: &Schema) {
    out.push_str(&format!(
        "    <Schema Namespace=\"{}\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">\n",
        schema.namespace
    ));

    for term in &schema.terms {
        emit_term(out, term);
    }
    for enum_type in &schema.enum_types {
        emit_enum_type(out, enum_type);
    }
    for entity_type in &schema.entity_types {
        emit_entity_type(out, entity_type);
    }
    for action in &schema.actions {
        emit_action(out, action);
    }
    for function in &schema.functions {
        emit_function(out, function);
    }
    for container in &schema.entity_containers {
        emit_entity_container(out, container);
    }

    out.push_str("    </Schema>\n");
}

fn emit_term(out: &mut String, term: &Term) {
    out.push_str(&format!(
        "      <Term Name=\"{}\" Type=\"{}\"",
        term.name, term.type_name
    ));
    if let Some(ref applies_to) = term.applies_to {
        out.push_str(&format!(" AppliesTo=\"{applies_to}\""));
    }
    if let Some(ref description) = term.description {
        out.push_str(&format!(" Description=\"{}\"", xml_escape(description)));
    }
    out.push_str("/>\n");
}

fn emit_enum_type(out: &mut String, et: &EnumType) {
    out.push_str(&format!("      <EnumType Name=\"{}\">\n", et.name));
    for member in &et.members {
        if let Some(val) = member.value {
            out.push_str(&format!(
                "        <Member Name=\"{}\" Value=\"{}\"/>\n",
                member.name, val
            ));
        } else {
            out.push_str(&format!("        <Member Name=\"{}\"/>\n", member.name));
        }
    }
    out.push_str("      </EnumType>\n");
}

fn emit_entity_type(out: &mut String, et: &EntityType) {
    if et.has_stream {
        out.push_str(&format!(
            "      <EntityType Name=\"{}\" HasStream=\"true\">\n",
            et.name
        ));
    } else {
        out.push_str(&format!("      <EntityType Name=\"{}\">\n", et.name));
    }

    // Key
    if !et.key_properties.is_empty() {
        out.push_str("        <Key>\n");
        for key in &et.key_properties {
            out.push_str(&format!("          <PropertyRef Name=\"{key}\"/>\n"));
        }
        out.push_str("        </Key>\n");
    }

    // Properties
    for prop in &et.properties {
        emit_property(out, prop);
    }

    // Navigation properties
    for nav in &et.navigation_properties {
        emit_navigation_property(out, nav);
    }

    // Annotations
    for ann in &et.annotations {
        emit_annotation(out, ann, 8);
    }

    out.push_str("      </EntityType>\n");
}

fn emit_property(out: &mut String, prop: &Property) {
    out.push_str(&format!(
        "        <Property Name=\"{}\" Type=\"{}\"",
        prop.name, prop.type_name
    ));
    if !prop.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(ref default) = prop.default_value {
        out.push_str(&format!(" DefaultValue=\"{default}\""));
    }
    if let Some(precision) = prop.precision {
        out.push_str(&format!(" Precision=\"{precision}\""));
    }
    if let Some(scale) = prop.scale {
        out.push_str(&format!(" Scale=\"{scale}\""));
    }
    out.push_str("/>\n");
}

fn emit_navigation_property(out: &mut String, nav: &NavigationProperty) {
    let has_children = !nav.referential_constraints.is_empty();

    out.push_str(&format!(
        "        <NavigationProperty Name=\"{}\" Type=\"{}\"",
        nav.name, nav.type_name
    ));
    if !nav.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if nav.contains_target {
        out.push_str(" ContainsTarget=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for rc in &nav.referential_constraints {
            out.push_str(&format!(
                "          <ReferentialConstraint Property=\"{}\" ReferencedProperty=\"{}\"/>\n",
                rc.property, rc.referenced_property
            ));
        }
        out.push_str("        </NavigationProperty>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_action(out: &mut String, action: &Action) {
    let has_children = !action.parameters.is_empty()
        || action.return_type.is_some()
        || !action.annotations.is_empty();

    out.push_str(&format!("      <Action Name=\"{}\"", action.name));
    if action.is_bound {
        out.push_str(" IsBound=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for param in &action.parameters {
            emit_parameter(out, param);
        }
        if let Some(ref rt) = action.return_type {
            emit_return_type(out, rt);
        }
        for ann in &action.annotations {
            emit_annotation(out, ann, 8);
        }
        out.push_str("      </Action>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_function(out: &mut String, func: &Function) {
    let has_children =
        !func.parameters.is_empty() || func.return_type.is_some() || !func.annotations.is_empty();

    out.push_str(&format!("      <Function Name=\"{}\"", func.name));
    if func.is_bound {
        out.push_str(" IsBound=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for param in &func.parameters {
            emit_parameter(out, param);
        }
        if let Some(ref rt) = func.return_type {
            emit_return_type(out, rt);
        }
        for ann in &func.annotations {
            emit_annotation(out, ann, 8);
        }
        out.push_str("      </Function>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_parameter(out: &mut String, param: &Parameter) {
    out.push_str(&format!(
        "        <Parameter Name=\"{}\" Type=\"{}\"",
        param.name, param.type_name
    ));
    if !param.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(ref default) = param.default_value {
        out.push_str(&format!(" DefaultValue=\"{default}\""));
    }
    out.push_str("/>\n");
}

fn emit_return_type(out: &mut String, rt: &ReturnType) {
    out.push_str(&format!("        <ReturnType Type=\"{}\"", rt.type_name));
    if !rt.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(precision) = rt.precision {
        out.push_str(&format!(" Precision=\"{precision}\""));
    }
    if let Some(scale) = rt.scale {
        out.push_str(&format!(" Scale=\"{scale}\""));
    }
    out.push_str("/>\n");
}

fn emit_entity_container(out: &mut String, container: &EntityContainer) {
    let has_children = !container.entity_sets.is_empty()
        || !container.action_imports.is_empty()
        || !container.function_imports.is_empty();

    out.push_str(&format!(
        "      <EntityContainer Name=\"{}\"",
        container.name
    ));

    if has_children {
        out.push_str(">\n");
        for es in &container.entity_sets {
            emit_entity_set(out, es);
        }
        for ai in &container.action_imports {
            out.push_str(&format!(
                "        <ActionImport Name=\"{}\" Action=\"{}\"/>\n",
                ai.name, ai.action
            ));
        }
        for fi in &container.function_imports {
            out.push_str(&format!(
                "        <FunctionImport Name=\"{}\" Function=\"{}\"/>\n",
                fi.name, fi.function
            ));
        }
        out.push_str("      </EntityContainer>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_entity_set(out: &mut String, es: &EntitySet) {
    if es.navigation_bindings.is_empty() {
        out.push_str(&format!(
            "        <EntitySet Name=\"{}\" EntityType=\"{}\"/>\n",
            es.name, es.entity_type
        ));
    } else {
        out.push_str(&format!(
            "        <EntitySet Name=\"{}\" EntityType=\"{}\">\n",
            es.name, es.entity_type
        ));
        for nb in &es.navigation_bindings {
            out.push_str(&format!(
                "          <NavigationPropertyBinding Path=\"{}\" Target=\"{}\"/>\n",
                nb.path, nb.target
            ));
        }
        out.push_str("        </EntitySet>\n");
    }
}

fn emit_annotation(out: &mut String, ann: &Annotation, indent: usize) {
    let pad: String = " ".repeat(indent);
    match &ann.value {
        AnnotationValue::String(s) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" String=\"{}\"/>\n",
                ann.term,
                xml_escape(s)
            ));
        }
        AnnotationValue::Float(f) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Float=\"{f}\"/>\n",
                ann.term
            ));
        }
        AnnotationValue::Bool(b) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Bool=\"{b}\"/>\n",
                ann.term
            ));
        }
        AnnotationValue::Int(i) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Int=\"{i}\"/>\n",
                ann.term
            ));
        }
        AnnotationValue::Collection(items) => {
            out.push_str(&format!("{pad}<Annotation Term=\"{}\">\n", ann.term));
            out.push_str(&format!("{pad}  <Collection>\n"));
            for item in items {
                out.push_str(&format!("{pad}    <String>{}</String>\n", xml_escape(item)));
            }
            out.push_str(&format!("{pad}  </Collection>\n"));
            out.push_str(&format!("{pad}</Annotation>\n"));
        }
        AnnotationValue::Record(map) => {
            out.push_str(&format!("{pad}<Annotation Term=\"{}\">\n", ann.term));
            out.push_str(&format!("{pad}  <Record>\n"));
            for (k, v) in map {
                out.push_str(&format!(
                    "{pad}    <PropertyValue Property=\"{k}\" String=\"{}\"/>\n",
                    xml_escape(v)
                ));
            }
            out.push_str(&format!("{pad}  </Record>\n"));
            out.push_str(&format!("{pad}</Annotation>\n"));
        }
    }
}

/// Escape XML special characters in attribute/text values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
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
}
