use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::types::*;

#[derive(Debug, thiserror::Error)]
pub enum CsdlParseError {
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("missing required attribute '{attr}' on element '{element}'")]
    MissingAttribute { element: String, attr: String },
    #[error("unexpected element: {0}")]
    UnexpectedElement(String),
    #[error("invalid CSDL: {0}")]
    Invalid(String),
}

/// Parse a CSDL XML document from a string.
pub fn parse_csdl(xml: &str) -> Result<CsdlDocument, CsdlParseError> {
    let mut reader = Reader::from_str(xml);
    let mut doc = CsdlDocument {
        version: String::new(),
        schemas: Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "Edmx" => doc.version = attr_str(e, "Version").unwrap_or_default(),
                    "Schema" => doc.schemas.push(parse_schema(&mut reader, e)?),
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                if local_name(e) == "Edmx" {
                    doc.version = attr_str(e, "Version").unwrap_or_default();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(doc)
}

/// Read children of a <Schema> element until </Schema>.
fn parse_schema(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Schema, CsdlParseError> {
    let namespace = required_attr(start, "Namespace")?;
    let mut schema = Schema {
        namespace,
        entity_types: Vec::new(),
        enum_types: Vec::new(),
        actions: Vec::new(),
        functions: Vec::new(),
        entity_containers: Vec::new(),
        terms: Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "EntityType" => schema.entity_types.push(parse_entity_type(reader, e)?),
                    "EnumType" => schema.enum_types.push(parse_enum_type(reader, e)?),
                    "Action" => schema.actions.push(parse_action(reader, e)?),
                    "Function" => schema.functions.push(parse_function(reader, e)?),
                    "EntityContainer" => schema.entity_containers.push(parse_entity_container(reader, e)?),
                    _ => { skip_element(reader)?; }
                }
            }
            Ok(Event::Empty(ref e)) => {
                match local_name(e).as_str() {
                    "Term" => schema.terms.push(parse_term(e)),
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "Schema" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(schema)
}

fn parse_entity_type(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EntityType, CsdlParseError> {
    let name = required_attr(start, "Name")?;
    let mut et = EntityType {
        name,
        key_properties: Vec::new(),
        properties: Vec::new(),
        navigation_properties: Vec::new(),
        annotations: Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "NavigationProperty" => {
                        et.navigation_properties.push(parse_navigation_property_children(reader, e)?);
                    }
                    "Annotation" => {
                        et.annotations.push(parse_annotation_children(reader, e)?);
                    }
                    "Key" => {
                        // Read Key's children (PropertyRef elements)
                        let mut kbuf = Vec::new();
                        loop {
                            match reader.read_event_into(&mut kbuf) {
                                Ok(Event::Empty(ref ke)) if local_name(ke) == "PropertyRef" => {
                                    if let Some(n) = attr_str(ke, "Name") {
                                        et.key_properties.push(n);
                                    }
                                }
                                Ok(Event::End(ref ke)) if local_name_end(ke) == "Key" => break,
                                Ok(Event::Eof) => break,
                                Err(e) => return Err(CsdlParseError::Xml(e)),
                                _ => {}
                            }
                            kbuf.clear();
                        }
                    }
                    _ => { skip_element(reader)?; }
                }
            }
            Ok(Event::Empty(ref e)) => {
                match local_name(e).as_str() {
                    "PropertyRef" => {
                        if let Some(n) = attr_str(e, "Name") {
                            et.key_properties.push(n);
                        }
                    }
                    "Property" => et.properties.push(parse_property(e)),
                    "NavigationProperty" => {
                        et.navigation_properties.push(nav_prop_from_attrs(e));
                    }
                    "Annotation" => {
                        if let Some(ann) = annotation_from_attrs(e) {
                            et.annotations.push(ann);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "EntityType" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(et)
}

fn parse_enum_type(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EnumType, CsdlParseError> {
    let name = required_attr(start, "Name")?;
    let mut members = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) if local_name(e) == "Member" => {
                members.push(EnumMember {
                    name: attr_str(e, "Name").unwrap_or_default(),
                    value: attr_str(e, "Value").and_then(|v| v.parse().ok()),
                });
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "EnumType" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(EnumType { name, members })
}

fn parse_action(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Action, CsdlParseError> {
    let name = required_attr(start, "Name")?;
    let is_bound = attr_str(start, "IsBound").is_some_and(|v| v == "true");
    let mut parameters = Vec::new();
    let mut return_type = None;
    let mut annotations = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "Annotation" => annotations.push(parse_annotation_children(reader, e)?),
                    _ => { skip_element(reader)?; }
                }
            }
            Ok(Event::Empty(ref e)) => {
                match local_name(e).as_str() {
                    "Parameter" => parameters.push(parse_parameter(e)),
                    "ReturnType" => return_type = Some(parse_return_type(e)),
                    "Annotation" => {
                        if let Some(ann) = annotation_from_attrs(e) {
                            annotations.push(ann);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "Action" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(Action { name, is_bound, parameters, return_type, annotations })
}

fn parse_function(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Function, CsdlParseError> {
    let name = required_attr(start, "Name")?;
    let is_bound = attr_str(start, "IsBound").is_some_and(|v| v == "true");
    let mut parameters = Vec::new();
    let mut return_type = None;
    let mut annotations = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "Annotation" => annotations.push(parse_annotation_children(reader, e)?),
                    _ => { skip_element(reader)?; }
                }
            }
            Ok(Event::Empty(ref e)) => {
                match local_name(e).as_str() {
                    "Parameter" => parameters.push(parse_parameter(e)),
                    "ReturnType" => return_type = Some(parse_return_type(e)),
                    "Annotation" => {
                        if let Some(ann) = annotation_from_attrs(e) {
                            annotations.push(ann);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "Function" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(Function { name, is_bound, parameters, return_type, annotations })
}

fn parse_entity_container(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EntityContainer, CsdlParseError> {
    let name = required_attr(start, "Name")?;
    let mut entity_sets = Vec::new();
    let mut action_imports = Vec::new();
    let mut function_imports = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match local_name(e).as_str() {
                    "EntitySet" => entity_sets.push(parse_entity_set_children(reader, e)?),
                    _ => { skip_element(reader)?; }
                }
            }
            Ok(Event::Empty(ref e)) => {
                match local_name(e).as_str() {
                    "EntitySet" => {
                        entity_sets.push(EntitySet {
                            name: attr_str(e, "Name").unwrap_or_default(),
                            entity_type: attr_str(e, "EntityType").unwrap_or_default(),
                            navigation_bindings: Vec::new(),
                        });
                    }
                    "ActionImport" => {
                        action_imports.push(ActionImport {
                            name: attr_str(e, "Name").unwrap_or_default(),
                            action: attr_str(e, "Action").unwrap_or_default(),
                        });
                    }
                    "FunctionImport" => {
                        function_imports.push(FunctionImport {
                            name: attr_str(e, "Name").unwrap_or_default(),
                            function: attr_str(e, "Function").unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "EntityContainer" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(EntityContainer { name, entity_sets, action_imports, function_imports })
}

// --- Element parsers for self-closing (Empty) elements ---

fn parse_property(e: &BytesStart) -> Property {
    Property {
        name: attr_str(e, "Name").unwrap_or_default(),
        type_name: attr_str(e, "Type").unwrap_or_else(|| "Edm.String".to_string()),
        nullable: attr_str(e, "Nullable").map_or(true, |v| v != "false"),
        default_value: attr_str(e, "DefaultValue"),
        precision: attr_str(e, "Precision").and_then(|v| v.parse().ok()),
        scale: attr_str(e, "Scale").and_then(|v| v.parse().ok()),
    }
}

fn parse_parameter(e: &BytesStart) -> Parameter {
    Parameter {
        name: attr_str(e, "Name").unwrap_or_default(),
        type_name: attr_str(e, "Type").unwrap_or_else(|| "Edm.String".to_string()),
        nullable: attr_str(e, "Nullable").map_or(true, |v| v != "false"),
        default_value: attr_str(e, "DefaultValue"),
    }
}

fn parse_return_type(e: &BytesStart) -> ReturnType {
    ReturnType {
        type_name: attr_str(e, "Type").unwrap_or_default(),
        nullable: attr_str(e, "Nullable").map_or(true, |v| v != "false"),
        precision: attr_str(e, "Precision").and_then(|v| v.parse().ok()),
        scale: attr_str(e, "Scale").and_then(|v| v.parse().ok()),
    }
}

fn parse_term(e: &BytesStart) -> Term {
    Term {
        name: attr_str(e, "Name").unwrap_or_default(),
        type_name: attr_str(e, "Type").unwrap_or_default(),
        applies_to: attr_str(e, "AppliesTo"),
        description: attr_str(e, "Description"),
    }
}

fn nav_prop_from_attrs(e: &BytesStart) -> NavigationProperty {
    NavigationProperty {
        name: attr_str(e, "Name").unwrap_or_default(),
        type_name: attr_str(e, "Type").unwrap_or_default(),
        nullable: attr_str(e, "Nullable").map_or(true, |v| v != "false"),
        contains_target: attr_str(e, "ContainsTarget").is_some_and(|v| v == "true"),
        referential_constraints: Vec::new(),
    }
}

/// Parse annotation from inline attributes only (for self-closing elements).
fn annotation_from_attrs(e: &BytesStart) -> Option<Annotation> {
    let term = attr_str(e, "Term")?;

    let value = if let Some(s) = attr_str(e, "String") {
        AnnotationValue::String(s)
    } else if let Some(f) = attr_str(e, "Float") {
        AnnotationValue::Float(f.parse().unwrap_or(0.0))
    } else if let Some(b) = attr_str(e, "Bool") {
        AnnotationValue::Bool(b == "true")
    } else if let Some(i) = attr_str(e, "Int") {
        AnnotationValue::Int(i.parse().unwrap_or(0))
    } else {
        AnnotationValue::String(String::new())
    };

    Some(Annotation { term, value })
}

// --- Element parsers for Start elements (have children) ---

fn parse_navigation_property_children(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<NavigationProperty, CsdlParseError> {
    let mut nav = nav_prop_from_attrs(start);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) if local_name(e) == "ReferentialConstraint" => {
                nav.referential_constraints.push(ReferentialConstraint {
                    property: attr_str(e, "Property").unwrap_or_default(),
                    referenced_property: attr_str(e, "ReferencedProperty").unwrap_or_default(),
                });
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "NavigationProperty" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(nav)
}

fn parse_entity_set_children(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EntitySet, CsdlParseError> {
    let name = attr_str(start, "Name").unwrap_or_default();
    let entity_type = attr_str(start, "EntityType").unwrap_or_default();
    let mut navigation_bindings = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) if local_name(e) == "NavigationPropertyBinding" => {
                navigation_bindings.push(NavigationBinding {
                    path: attr_str(e, "Path").unwrap_or_default(),
                    target: attr_str(e, "Target").unwrap_or_default(),
                });
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "EntitySet" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(EntitySet { name, entity_type, navigation_bindings })
}

/// Parse annotation with nested children (Collection, String elements).
fn parse_annotation_children(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Annotation, CsdlParseError> {
    let term = attr_str(start, "Term").unwrap_or_default();

    // Check inline attributes first
    if let Some(s) = attr_str(start, "String") {
        // Has inline value but also has children (e.g., multi-line) — skip children
        skip_element(reader)?;
        return Ok(Annotation { term, value: AnnotationValue::String(s) });
    }
    if let Some(f) = attr_str(start, "Float") {
        skip_element(reader)?;
        return Ok(Annotation { term, value: AnnotationValue::Float(f.parse().unwrap_or(0.0)) });
    }

    // Read children
    let mut collection_items = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if local_name(e) == "String" => {
                let text = reader.read_text(e.name()).unwrap_or_default();
                let text = text.trim().to_string();
                if !text.is_empty() {
                    collection_items.push(text);
                }
            }
            Ok(Event::End(ref e)) if local_name_end(e) == "Annotation" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }

    let value = if !collection_items.is_empty() {
        AnnotationValue::Collection(collection_items)
    } else {
        AnnotationValue::String(String::new())
    };

    Ok(Annotation { term, value })
}

// --- Utilities ---

/// Skip past all children of the current element (consume until matching End).
fn skip_element(reader: &mut Reader<&[u8]>) -> Result<(), CsdlParseError> {
    let mut depth: u32 = 1;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn local_name(e: &BytesStart) -> String {
    let name = e.name();
    let full = std::str::from_utf8(name.as_ref()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full).to_string()
}

fn local_name_end(e: &quick_xml::events::BytesEnd) -> String {
    let name = e.name();
    let full = std::str::from_utf8(name.as_ref()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full).to_string()
}

fn attr_str(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| std::str::from_utf8(a.key.as_ref()).unwrap_or("") == name)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
}

fn required_attr(e: &BytesStart, name: &str) -> Result<String, CsdlParseError> {
    attr_str(e, name).ok_or_else(|| CsdlParseError::MissingAttribute {
        element: local_name(e),
        attr: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_reference_ecommerce_csdl() {
        let xml = include_str!("../../../../reference/ecommerce/specs/model.csdl.xml");
        let doc = parse_csdl(xml).expect("should parse without error");

        assert_eq!(doc.version, "4.0");
        assert_eq!(doc.schemas.len(), 2);

        let schema = doc.schemas.iter()
            .find(|s| s.namespace == "Temper.Ecommerce")
            .expect("should have Temper.Ecommerce schema");

        // Entity types
        assert_eq!(schema.entity_types.len(), 7);
        assert!(schema.entity_type("Customer").is_some());
        assert!(schema.entity_type("Order").is_some());
        assert!(schema.entity_type("Product").is_some());
        assert!(schema.entity_type("Payment").is_some());
        assert!(schema.entity_type("Shipment").is_some());
        assert!(schema.entity_type("OrderItem").is_some());
        assert!(schema.entity_type("Address").is_some());

        // Enum types
        assert_eq!(schema.enum_types.len(), 3);

        // Order details
        let order = schema.entity_type("Order").unwrap();
        assert_eq!(order.key_properties, vec!["Id"]);
        assert!(order.properties.len() > 10);

        let states = order.state_machine_states().expect("should have states");
        assert_eq!(states.len(), 10);
        assert!(states.contains(&"Draft".to_string()));
        assert!(states.contains(&"Shipped".to_string()));
        assert_eq!(order.initial_state(), Some("Draft".to_string()));
        assert_eq!(order.tla_spec_path(), Some("order.tla".to_string()));

        // Navigation properties
        let customer_nav = order.navigation_properties.iter().find(|n| n.name == "Customer").unwrap();
        assert!(!customer_nav.contains_target);
        assert_eq!(customer_nav.referential_constraints.len(), 1);

        let items_nav = order.navigation_properties.iter().find(|n| n.name == "Items").unwrap();
        assert!(items_nav.contains_target);

        // Actions
        let submit = schema.action("SubmitOrder").unwrap();
        assert!(submit.is_bound);
        assert_eq!(submit.valid_from_states(), Some(vec!["Draft".to_string()]));
        assert_eq!(submit.target_state(), Some("Submitted".to_string()));

        let cancel = schema.action("CancelOrder").unwrap();
        let cancel_from = cancel.valid_from_states().unwrap();
        assert_eq!(cancel_from.len(), 3);

        // Functions
        assert!(schema.function("GetOrderTotal").unwrap().is_bound);
        assert!(!schema.function("SearchProducts").unwrap().is_bound);

        // Entity container
        let container = &schema.entity_containers[0];
        assert_eq!(container.name, "EcommerceService");
        assert_eq!(container.entity_sets.len(), 5);

        let orders_set = container.entity_sets.iter().find(|s| s.name == "Orders").unwrap();
        assert_eq!(orders_set.entity_type, "Temper.Ecommerce.Order");
        assert_eq!(orders_set.navigation_bindings.len(), 3);
    }

    #[test]
    fn test_parse_minimal_csdl() {
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
        let schema = &doc.schemas[0];
        assert_eq!(schema.namespace, "Test");
        let widget = schema.entity_type("Widget").unwrap();
        assert_eq!(widget.key_properties, vec!["Id"]);
        assert_eq!(widget.properties.len(), 2);
    }
}
