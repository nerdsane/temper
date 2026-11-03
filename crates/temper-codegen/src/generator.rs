//! Top-level code generator that orchestrates entity, message, and state machine generation.

use temper_spec::csdl::{Action, Function, Schema};
use temper_spec::model::SpecModel;

use crate::entity;
use crate::messages;
use crate::state_machine;

/// Errors that can occur during code generation.
#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    /// The requested entity type was not found in the CSDL model.
    #[error("entity type '{0}' not found in CSDL")]
    EntityNotFound(String),
    /// No domain schema was found in the CSDL document.
    #[error("no domain schema found")]
    NoDomainSchema,
    /// A general code generation error.
    #[error("codegen error: {0}")]
    Other(String),
}

/// A generated Rust module for a single entity.
#[derive(Debug)]
pub struct GeneratedModule {
    /// The entity name (e.g., "Order").
    pub entity_name: String,
    /// The generated Rust source code.
    pub source: String,
}

/// Generate a complete Rust module for an entity from the unified spec model.
pub fn generate_entity_module(
    spec: &SpecModel,
    entity_name: &str,
) -> Result<GeneratedModule, CodegenError> {
    // Find the domain schema (skip vocabulary schemas)
    let schema = spec
        .csdl
        .schemas
        .iter()
        .find(|s| s.entity_types.iter().any(|e| e.name == entity_name))
        .ok_or_else(|| CodegenError::EntityNotFound(entity_name.to_string()))?;

    let entity = schema
        .entity_type(entity_name)
        .ok_or_else(|| CodegenError::EntityNotFound(entity_name.to_string()))?;

    let namespace = &schema.namespace;

    let mut source = String::new();

    // Module header
    source.push_str(&format!(
        "//! Generated code for the {} entity actor.\n",
        entity_name
    ));
    source.push_str("//! DO NOT EDIT — regenerate from specs with `temper codegen`.\n\n");
    source.push_str("#![allow(dead_code)]\n\n");
    source.push_str("use serde::{Serialize, Deserialize};\n");
    source.push_str("use uuid::Uuid;\n");
    source.push_str("use chrono::{DateTime, Utc};\n\n");

    // Entity state struct
    source.push_str(&entity::generate_entity_struct(entity, namespace));
    source.push('\n');

    // Default impl
    source.push_str(&entity::generate_entity_default(entity));
    source.push('\n');

    // State machine (if TLA+ spec is available)
    if let Some(sm) = spec.state_machines.get(entity_name) {
        source.push_str(&state_machine::generate_state_machine(entity_name, sm));
        source.push('\n');
    }

    // Message enum
    let bound_actions = find_bound_actions(schema, entity_name, namespace);
    let bound_functions = find_bound_functions(schema, entity_name, namespace);
    source.push_str(&messages::generate_message_enum(
        entity_name,
        &bound_actions,
        &bound_functions,
        namespace,
    ));

    Ok(GeneratedModule {
        entity_name: entity_name.to_string(),
        source,
    })
}

/// Find all actions bound to a given entity type.
fn find_bound_actions<'a>(
    schema: &'a Schema,
    entity_name: &str,
    namespace: &str,
) -> Vec<&'a Action> {
    let full_type = format!("{}.{}", namespace, entity_name);
    schema
        .actions
        .iter()
        .filter(|a| {
            a.is_bound
                && a.parameters
                    .first()
                    .is_some_and(|p| p.type_name == full_type)
        })
        .collect()
}

/// Find all functions bound to a given entity type.
fn find_bound_functions<'a>(
    schema: &'a Schema,
    entity_name: &str,
    namespace: &str,
) -> Vec<&'a Function> {
    let full_type = format!("{}.{}", namespace, entity_name);
    schema
        .functions
        .iter()
        .filter(|f| {
            f.is_bound
                && f.parameters
                    .first()
                    .is_some_and(|p| p.type_name == full_type)
        })
        .collect()
}
