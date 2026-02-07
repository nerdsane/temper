//! Top-level code generator that orchestrates entity, message, and state machine generation.

use temper_spec::csdl::{Action, Function, Schema};
use temper_spec::model::SpecModel;

use crate::entity;
use crate::messages;
use crate::state_machine;

#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    #[error("entity type '{0}' not found in CSDL")]
    EntityNotFound(String),
    #[error("no domain schema found")]
    NoDomainSchema,
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
fn find_bound_actions<'a>(schema: &'a Schema, entity_name: &str, namespace: &str) -> Vec<&'a Action> {
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
fn find_bound_functions<'a>(schema: &'a Schema, entity_name: &str, namespace: &str) -> Vec<&'a Function> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    fn load_reference_spec() -> SpecModel {
        let csdl_xml = include_str!("../../../reference/ecommerce/specs/model.csdl.xml");
        let order_tla = include_str!("../../../reference/ecommerce/specs/order.tla");

        let csdl = parse_csdl(csdl_xml).unwrap();
        let mut tla_sources = HashMap::new();
        tla_sources.insert("Order".to_string(), order_tla.to_string());

        build_spec_model(csdl, tla_sources)
    }

    #[test]
    fn test_generate_order_module() {
        let spec = load_reference_spec();
        let module = generate_entity_module(&spec, "Order").unwrap();

        assert_eq!(module.entity_name, "Order");

        // Should contain the entity struct
        assert!(module.source.contains("pub struct OrderState {"));
        assert!(module.source.contains("pub id: Uuid"));
        assert!(module.source.contains("pub status: OrderStatus"));
        assert!(module.source.contains("pub total:"));

        // Should contain the state machine enum
        assert!(module.source.contains("pub enum OrderStatus {"));
        assert!(module.source.contains("Draft,"));
        assert!(module.source.contains("Submitted,"));
        assert!(module.source.contains("Shipped,"));
        assert!(module.source.contains("Refunded,"));

        // Should contain the transition table
        assert!(module.source.contains("pub fn can_transition("));
        assert!(module.source.contains("\"SubmitOrder\""));
        assert!(module.source.contains("\"CancelOrder\""));

        // Should contain the message enum
        assert!(module.source.contains("pub enum OrderMsg {"));
        assert!(module.source.contains("SubmitOrder {"));
        assert!(module.source.contains("CancelOrder {"));
        assert!(module.source.contains("GetState,"));

        // Should contain bound functions
        assert!(module.source.contains("GetOrderTotal"));

        // Should have invariant names
        assert!(module.source.contains("TypeInvariant"));
        assert!(module.source.contains("ShipRequiresPayment"));
    }

    #[test]
    fn test_generate_customer_module() {
        let spec = load_reference_spec();
        let module = generate_entity_module(&spec, "Customer").unwrap();

        assert!(module.source.contains("pub struct CustomerState {"));
        assert!(module.source.contains("pub email: String"));
        // Customer has GetOrderHistory and GetRecommendations functions
        assert!(module.source.contains("pub enum CustomerMsg {"));
        assert!(module.source.contains("GetOrderHistory"));
        assert!(module.source.contains("GetRecommendations"));
    }

    #[test]
    fn test_generate_nonexistent_entity_fails() {
        let spec = load_reference_spec();
        let result = generate_entity_module(&spec, "NonExistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_generated_code_contains_no_stubs() {
        let spec = load_reference_spec();
        let module = generate_entity_module(&spec, "Order").unwrap();

        // Should not contain TODO or stub markers
        assert!(!module.source.contains("todo!"));
        assert!(!module.source.contains("unimplemented!"));
    }
}
