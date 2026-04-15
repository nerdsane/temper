use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::super::types::*;
use super::CsdlParseError;
use super::elements::{
    annotation_from_attrs, nav_prop_from_attrs, parse_action_import, parse_annotation_children,
    parse_entity_set_children, parse_entity_set_empty, parse_function_import,
    parse_navigation_property_children, parse_parameter, parse_property, parse_return_type,
    parse_term,
};
use super::xml::{attr_str, local_name, local_name_end, required_attr, skip_element};

pub(super) fn parse_schema(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<Schema, CsdlParseError> {
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
            Ok(Event::Start(ref element)) => match local_name(element).as_str() {
                "EntityType" => schema
                    .entity_types
                    .push(parse_entity_type(reader, element)?),
                "EnumType" => schema.enum_types.push(parse_enum_type(reader, element)?),
                "Action" => schema.actions.push(parse_action(reader, element)?),
                "Function" => schema.functions.push(parse_function(reader, element)?),
                "EntityContainer" => schema
                    .entity_containers
                    .push(parse_entity_container(reader, element)?),
                _ => skip_element(reader)?,
            },
            Ok(Event::Empty(ref element)) if local_name(element) == "Term" => {
                schema.terms.push(parse_term(element));
            }
            Ok(Event::End(ref element)) if local_name_end(element) == "Schema" => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(schema)
}

fn parse_entity_type(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<EntityType, CsdlParseError> {
    let mut entity_type = EntityType {
        name: required_attr(start, "Name")?,
        key_properties: Vec::new(),
        properties: Vec::new(),
        navigation_properties: Vec::new(),
        annotations: Vec::new(),
        has_stream: attr_str(start, "HasStream").is_some_and(|v| v == "true"),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref element)) => match local_name(element).as_str() {
                "NavigationProperty" => entity_type
                    .navigation_properties
                    .push(parse_navigation_property_children(reader, element)?),
                "Annotation" => entity_type
                    .annotations
                    .push(parse_annotation_children(reader, element)?),
                "Key" => parse_key(reader, &mut entity_type.key_properties)?,
                _ => skip_element(reader)?,
            },
            Ok(Event::Empty(ref element)) => match local_name(element).as_str() {
                "PropertyRef" => push_property_ref(element, &mut entity_type.key_properties),
                "Property" => entity_type.properties.push(parse_property(element)),
                "NavigationProperty" => {
                    entity_type
                        .navigation_properties
                        .push(nav_prop_from_attrs(element));
                }
                "Annotation" => {
                    if let Some(annotation) = annotation_from_attrs(element) {
                        entity_type.annotations.push(annotation);
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref element)) if local_name_end(element) == "EntityType" => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(entity_type)
}

fn parse_key(
    reader: &mut Reader<&[u8]>,
    key_properties: &mut Vec<String>,
) -> Result<(), CsdlParseError> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref element)) if local_name(element) == "PropertyRef" => {
                push_property_ref(element, key_properties);
            }
            Ok(Event::End(ref element)) if local_name_end(element) == "Key" => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(())
}

fn push_property_ref(element: &BytesStart, key_properties: &mut Vec<String>) {
    if let Some(name) = attr_str(element, "Name") {
        key_properties.push(name);
    }
}

fn parse_enum_type(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<EnumType, CsdlParseError> {
    let mut members = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref element)) if local_name(element) == "Member" => {
                members.push(EnumMember {
                    name: attr_str(element, "Name").unwrap_or_default(),
                    value: attr_str(element, "Value").and_then(|v| v.parse().ok()),
                });
            }
            Ok(Event::End(ref element)) if local_name_end(element) == "EnumType" => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(EnumType {
        name: required_attr(start, "Name")?,
        members,
    })
}

fn parse_action(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Action, CsdlParseError> {
    parse_operation(
        reader,
        start,
        |name, is_bound, parameters, return_type, annotations| Action {
            name,
            is_bound,
            parameters,
            return_type,
            annotations,
        },
    )
}

fn parse_function(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<Function, CsdlParseError> {
    parse_operation(
        reader,
        start,
        |name, is_bound, parameters, return_type, annotations| Function {
            name,
            is_bound,
            parameters,
            return_type,
            annotations,
        },
    )
}

fn parse_operation<T, F>(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
    build: F,
) -> Result<T, CsdlParseError>
where
    F: FnOnce(String, bool, Vec<Parameter>, Option<ReturnType>, Vec<Annotation>) -> T,
{
    let name = required_attr(start, "Name")?;
    let is_bound = attr_str(start, "IsBound").is_some_and(|v| v == "true");
    let mut parameters = Vec::new();
    let mut return_type = None;
    let mut annotations = Vec::new();
    let end_name = local_name(start);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref element)) => match local_name(element).as_str() {
                "Annotation" => annotations.push(parse_annotation_children(reader, element)?),
                _ => skip_element(reader)?,
            },
            Ok(Event::Empty(ref element)) => match local_name(element).as_str() {
                "Parameter" => parameters.push(parse_parameter(element)),
                "ReturnType" => return_type = Some(parse_return_type(element)),
                "Annotation" => {
                    if let Some(annotation) = annotation_from_attrs(element) {
                        annotations.push(annotation);
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref element)) if local_name_end(element) == end_name => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(build(name, is_bound, parameters, return_type, annotations))
}

fn parse_entity_container(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<EntityContainer, CsdlParseError> {
    let mut entity_container = EntityContainer {
        name: required_attr(start, "Name")?,
        entity_sets: Vec::new(),
        action_imports: Vec::new(),
        function_imports: Vec::new(),
    };
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref element)) => match local_name(element).as_str() {
                "EntitySet" => entity_container
                    .entity_sets
                    .push(parse_entity_set_children(reader, element)?),
                _ => skip_element(reader)?,
            },
            Ok(Event::Empty(ref element)) => match local_name(element).as_str() {
                "EntitySet" => entity_container
                    .entity_sets
                    .push(parse_entity_set_empty(element)),
                "ActionImport" => entity_container
                    .action_imports
                    .push(parse_action_import(element)),
                "FunctionImport" => entity_container
                    .function_imports
                    .push(parse_function_import(element)),
                _ => {}
            },
            Ok(Event::End(ref element)) if local_name_end(element) == "EntityContainer" => break,
            Ok(Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(entity_container)
}
