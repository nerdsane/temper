//! Canonical ordering and duplicate validation for CSDL bundle inputs.

use std::collections::BTreeSet;

use crate::csdl::{
    Action, Annotation, CsdlDocument, EntityContainer, EntityType, Function, Schema, emit_csdl_xml,
    parse_csdl,
};

use super::{BundleError, BundleErrorCode};

pub(super) fn canonical_csdl(source: &str) -> Result<String, BundleError> {
    let mut document = parse_csdl(source).map_err(|error| {
        BundleError::new(
            BundleErrorCode::InvalidCsdl,
            format!("failed to parse CSDL: {error}"),
        )
    })?;
    if document.version.is_empty() || document.schemas.is_empty() {
        return Err(BundleError::new(
            BundleErrorCode::InvalidCsdl,
            "CSDL must declare an Edmx version and at least one schema",
        ));
    }
    canonicalize_document(&mut document)?;
    Ok(emit_csdl_xml(&document))
}

fn canonicalize_document(document: &mut CsdlDocument) -> Result<(), BundleError> {
    ensure_unique(
        "CSDL namespace",
        document.schemas.iter().map(|s| s.namespace.as_str()),
    )?;
    for schema in &mut document.schemas {
        canonicalize_schema(schema)?;
    }
    document
        .schemas
        .sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(())
}

fn canonicalize_schema(schema: &mut Schema) -> Result<(), BundleError> {
    let namespace = schema.namespace.as_str();
    ensure_unique_named(
        namespace,
        "entity type",
        schema.entity_types.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        namespace,
        "enum type",
        schema.enum_types.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        namespace,
        "entity container",
        schema.entity_containers.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        namespace,
        "term",
        schema.terms.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        namespace,
        "action overload",
        schema.actions.iter().map(action_signature),
    )?;
    ensure_unique_named(
        namespace,
        "function overload",
        schema.functions.iter().map(function_signature),
    )?;

    for entity in &mut schema.entity_types {
        canonicalize_entity(namespace, entity)?;
    }
    for enum_type in &mut schema.enum_types {
        ensure_unique_named(
            &format!("{namespace}.{}", enum_type.name),
            "enum member",
            enum_type.members.iter().map(|member| member.name.as_str()),
        )?;
        enum_type
            .members
            .sort_by(|left, right| left.name.cmp(&right.name));
    }
    for action in &mut schema.actions {
        canonicalize_annotations(namespace, &action.name, &mut action.annotations)?;
    }
    for function in &mut schema.functions {
        canonicalize_annotations(namespace, &function.name, &mut function.annotations)?;
    }
    for container in &mut schema.entity_containers {
        canonicalize_container(namespace, container)?;
    }

    schema
        .entity_types
        .sort_by(|left, right| left.name.cmp(&right.name));
    schema
        .enum_types
        .sort_by(|left, right| left.name.cmp(&right.name));
    schema.actions.sort_by_key(action_signature);
    schema.functions.sort_by_key(function_signature);
    schema
        .entity_containers
        .sort_by(|left, right| left.name.cmp(&right.name));
    schema
        .terms
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn canonicalize_entity(namespace: &str, entity: &mut EntityType) -> Result<(), BundleError> {
    let owner = format!("{namespace}.{}", entity.name);
    let mut member_names = entity
        .properties
        .iter()
        .map(|property| property.name.as_str())
        .collect::<Vec<_>>();
    member_names.extend(
        entity
            .navigation_properties
            .iter()
            .map(|property| property.name.as_str()),
    );
    ensure_unique_named(&owner, "property", member_names.into_iter())?;
    canonicalize_annotations(namespace, &entity.name, &mut entity.annotations)?;
    entity
        .properties
        .sort_by(|left, right| left.name.cmp(&right.name));
    entity
        .navigation_properties
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn canonicalize_annotations(
    namespace: &str,
    owner: &str,
    annotations: &mut [Annotation],
) -> Result<(), BundleError> {
    ensure_unique_named(
        &format!("{namespace}.{owner}"),
        "annotation",
        annotations
            .iter()
            .map(|annotation| annotation.term.as_str()),
    )?;
    annotations.sort_by(|left, right| left.term.cmp(&right.term));
    Ok(())
}

fn canonicalize_container(
    namespace: &str,
    container: &mut EntityContainer,
) -> Result<(), BundleError> {
    let owner = format!("{namespace}.{}", container.name);
    ensure_unique_named(
        &owner,
        "entity set",
        container.entity_sets.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        &owner,
        "action import",
        container.action_imports.iter().map(|v| v.name.as_str()),
    )?;
    ensure_unique_named(
        &owner,
        "function import",
        container.function_imports.iter().map(|v| v.name.as_str()),
    )?;
    for entity_set in &mut container.entity_sets {
        entity_set
            .navigation_bindings
            .sort_by(|left, right| (&left.path, &left.target).cmp(&(&right.path, &right.target)));
    }
    container
        .entity_sets
        .sort_by(|left, right| left.name.cmp(&right.name));
    container
        .action_imports
        .sort_by(|left, right| left.name.cmp(&right.name));
    container
        .function_imports
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn action_signature(action: &Action) -> String {
    operation_signature(
        &action.name,
        action
            .parameters
            .iter()
            .map(|value| value.type_name.as_str()),
    )
}

fn function_signature(function: &Function) -> String {
    operation_signature(
        &function.name,
        function
            .parameters
            .iter()
            .map(|value| value.type_name.as_str()),
    )
}

fn operation_signature<'a>(name: &str, parameters: impl Iterator<Item = &'a str>) -> String {
    format!("{name}({})", parameters.collect::<Vec<_>>().join(","))
}

fn ensure_unique<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> Result<(), BundleError> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(duplicate(kind, name));
        }
    }
    Ok(())
}

fn ensure_unique_named<'a>(
    owner: &str,
    kind: &str,
    names: impl Iterator<Item = impl AsRef<str> + 'a>,
) -> Result<(), BundleError> {
    let names = names
        .map(|name| format!("{owner}.{}", name.as_ref()))
        .collect::<Vec<_>>();
    ensure_unique(kind, names.iter().map(String::as_str))
}

fn duplicate(kind: &str, name: &str) -> BundleError {
    BundleError::new(
        BundleErrorCode::DuplicateSymbol,
        format!("duplicate {kind} '{name}'"),
    )
}
