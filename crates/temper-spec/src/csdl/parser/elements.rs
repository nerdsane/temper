use quick_xml::Reader;
use quick_xml::events::BytesStart;

use super::super::types::*;
use super::CsdlParseError;
use super::xml::{attr_str, local_name, local_name_end, skip_element};

pub(super) fn parse_property(element: &BytesStart) -> Property {
    Property {
        name: attr_str(element, "Name").unwrap_or_default(),
        type_name: attr_str(element, "Type").unwrap_or_else(|| "Edm.String".to_string()),
        nullable: attr_str(element, "Nullable").is_none_or(|v| v != "false"),
        default_value: attr_str(element, "DefaultValue"),
        precision: attr_str(element, "Precision").and_then(|v| v.parse().ok()),
        scale: attr_str(element, "Scale").and_then(|v| v.parse().ok()),
    }
}

pub(super) fn parse_parameter(element: &BytesStart) -> Parameter {
    Parameter {
        name: attr_str(element, "Name").unwrap_or_default(),
        type_name: attr_str(element, "Type").unwrap_or_else(|| "Edm.String".to_string()),
        nullable: attr_str(element, "Nullable").is_none_or(|v| v != "false"),
        default_value: attr_str(element, "DefaultValue"),
    }
}

pub(super) fn parse_return_type(element: &BytesStart) -> ReturnType {
    ReturnType {
        type_name: attr_str(element, "Type").unwrap_or_default(),
        nullable: attr_str(element, "Nullable").is_none_or(|v| v != "false"),
        precision: attr_str(element, "Precision").and_then(|v| v.parse().ok()),
        scale: attr_str(element, "Scale").and_then(|v| v.parse().ok()),
    }
}

pub(super) fn parse_term(element: &BytesStart) -> Term {
    Term {
        name: attr_str(element, "Name").unwrap_or_default(),
        type_name: attr_str(element, "Type").unwrap_or_default(),
        applies_to: attr_str(element, "AppliesTo"),
        description: attr_str(element, "Description"),
    }
}

pub(super) fn parse_entity_set_empty(element: &BytesStart) -> EntitySet {
    EntitySet {
        name: attr_str(element, "Name").unwrap_or_default(),
        entity_type: attr_str(element, "EntityType").unwrap_or_default(),
        navigation_bindings: Vec::new(),
    }
}

pub(super) fn parse_action_import(element: &BytesStart) -> ActionImport {
    ActionImport {
        name: attr_str(element, "Name").unwrap_or_default(),
        action: attr_str(element, "Action").unwrap_or_default(),
    }
}

pub(super) fn parse_function_import(element: &BytesStart) -> FunctionImport {
    FunctionImport {
        name: attr_str(element, "Name").unwrap_or_default(),
        function: attr_str(element, "Function").unwrap_or_default(),
    }
}

pub(super) fn nav_prop_from_attrs(element: &BytesStart) -> NavigationProperty {
    NavigationProperty {
        name: attr_str(element, "Name").unwrap_or_default(),
        type_name: attr_str(element, "Type").unwrap_or_default(),
        nullable: attr_str(element, "Nullable").is_none_or(|v| v != "false"),
        contains_target: attr_str(element, "ContainsTarget").is_some_and(|v| v == "true"),
        referential_constraints: Vec::new(),
        delete_policy: None,
    }
}

/// Read a navigation property's delete policy annotation. Any other annotation
/// on a navigation property is ignored, as before.
fn read_delete_policy(
    navigation_property: &mut NavigationProperty,
    annotation: Annotation,
) -> Result<(), CsdlParseError> {
    let short_term = DELETE_POLICY_TERM.trim_start_matches("Temper.Vocab.");
    if annotation.term != DELETE_POLICY_TERM
        && !annotation.term.ends_with(&format!(".{short_term}"))
    {
        return Ok(());
    }
    let invalid = |reason: String| {
        CsdlParseError::Invalid(format!(
            "NavigationProperty '{}': {DELETE_POLICY_TERM} {reason}",
            navigation_property.name
        ))
    };
    let AnnotationValue::String(value) = annotation.value else {
        return Err(invalid("must be a String".to_string()));
    };
    let policy: crate::cross_invariant::DeletePolicy = value.parse().map_err(invalid)?;
    if !policy.is_implemented() {
        return Err(invalid(format!("'{value}' is not implemented")));
    }
    navigation_property.delete_policy = Some(policy);
    Ok(())
}

pub(super) fn annotation_from_attrs(element: &BytesStart) -> Option<Annotation> {
    let term = attr_str(element, "Term")?;
    let value = parse_inline_annotation_value(element);
    Some(Annotation { term, value })
}

pub(super) fn parse_navigation_property_children(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<NavigationProperty, CsdlParseError> {
    let mut navigation_property = nav_prop_from_attrs(start);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "ReferentialConstraint" =>
            {
                navigation_property
                    .referential_constraints
                    .push(ReferentialConstraint {
                        property: attr_str(element, "Property").unwrap_or_default(),
                        referenced_property: attr_str(element, "ReferencedProperty")
                            .unwrap_or_default(),
                    });
            }
            Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "Annotation" =>
            {
                if let Some(annotation) = annotation_from_attrs(element) {
                    read_delete_policy(&mut navigation_property, annotation)?;
                }
            }
            Ok(quick_xml::events::Event::Start(ref element))
                if local_name(element) == "Annotation" =>
            {
                let annotation = parse_annotation_children(reader, element)?;
                read_delete_policy(&mut navigation_property, annotation)?;
            }
            Ok(quick_xml::events::Event::End(ref element))
                if local_name_end(element) == "NavigationProperty" =>
            {
                break;
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(navigation_property)
}

pub(super) fn parse_entity_set_children(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<EntitySet, CsdlParseError> {
    let mut entity_set = parse_entity_set_empty(start);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "NavigationPropertyBinding" =>
            {
                entity_set.navigation_bindings.push(NavigationBinding {
                    path: attr_str(element, "Path").unwrap_or_default(),
                    target: attr_str(element, "Target").unwrap_or_default(),
                });
            }
            Ok(quick_xml::events::Event::End(ref element))
                if local_name_end(element) == "EntitySet" =>
            {
                break;
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(entity_set)
}

pub(super) fn parse_annotation_children(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<Annotation, CsdlParseError> {
    let term = attr_str(start, "Term").unwrap_or_default();

    if let Some(value) = parse_inline_annotation_override(start) {
        skip_element(reader)?;
        return Ok(Annotation { term, value });
    }

    let mut collection_items = Vec::new();
    let mut in_collection = false;
    let mut record_fields: Option<std::collections::HashMap<String, String>> = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref element))
            | Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "Collection" =>
            {
                in_collection = true;
            }
            // <Record><PropertyValue Property="k" String="v"/>…</Record>: the
            // shape the emitter writes for a Record value. A Record with no
            // PropertyValue is still a Record, not an empty String.
            Ok(quick_xml::events::Event::Start(ref element))
            | Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "Record" =>
            {
                record_fields.get_or_insert_with(Default::default);
            }
            Ok(quick_xml::events::Event::Empty(ref element))
                if local_name(element) == "PropertyValue" =>
            {
                if let (Some(fields), Some(key)) =
                    (record_fields.as_mut(), attr_str(element, "Property"))
                {
                    fields.insert(key, attr_str(element, "String").unwrap_or_default());
                }
            }
            Ok(quick_xml::events::Event::Start(ref element)) if local_name(element) == "String" => {
                // quick-xml 0.41 changed `read_text` to return a `BytesText`
                // instead of an owned `String`; call `.decode()` to get the text.
                // Behavior is preserved exactly: 0.37's `read_text` also only
                // charset-decoded and did NOT unescape XML entities (its rustdoc:
                // "does not unescape read data"), matching the inline-attribute
                // path (`attr_str`). Proper entity unescaping here (and the
                // parse->emit double-escape it would fix) is a separate pre-existing
                // correctness gap, out of scope for a dependency bump.
                let text = reader
                    .read_text(element.name())
                    .ok()
                    .and_then(|raw| raw.decode().ok().map(|s| s.trim().to_string()))
                    .unwrap_or_default();
                if !text.is_empty() {
                    collection_items.push(text);
                }
            }
            Ok(quick_xml::events::Event::End(ref element))
                if local_name_end(element) == "Annotation" =>
            {
                break;
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    let value = if let Some(fields) = record_fields {
        AnnotationValue::Record(fields)
    } else if collection_items.is_empty() {
        AnnotationValue::String(String::new())
    } else if !in_collection && collection_items.len() == 1 {
        // `<Annotation Term="t"><String>v</String></Annotation>` is the element
        // form of `String="v"`: one String, not a Collection of one.
        AnnotationValue::String(collection_items.remove(0))
    } else {
        AnnotationValue::Collection(collection_items)
    };

    Ok(Annotation { term, value })
}

/// An inline value on an element that also has children: the attribute wins
/// and the children are skipped. Same four value attributes as the
/// self-closing form, so `<Annotation Term="t" Bool="true"></Annotation>`
/// reads the same as `<Annotation Term="t" Bool="true"/>`.
fn parse_inline_annotation_override(element: &BytesStart) -> Option<AnnotationValue> {
    ["String", "Float", "Bool", "Int"]
        .iter()
        .any(|name| attr_str(element, name).is_some())
        .then(|| parse_inline_annotation_value(element))
}

fn parse_inline_annotation_value(element: &BytesStart) -> AnnotationValue {
    if let Some(string_value) = attr_str(element, "String") {
        return AnnotationValue::String(string_value);
    }
    if let Some(float_value) = attr_str(element, "Float") {
        return AnnotationValue::Float(float_value.parse().unwrap_or(0.0));
    }
    if let Some(bool_value) = attr_str(element, "Bool") {
        return AnnotationValue::Bool(bool_value == "true");
    }
    if let Some(int_value) = attr_str(element, "Int") {
        return AnnotationValue::Int(int_value.parse().unwrap_or(0));
    }

    AnnotationValue::String(String::new())
}
