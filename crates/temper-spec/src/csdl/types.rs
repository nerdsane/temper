use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A complete CSDL document (parsed from edmx:Edmx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsdlDocument {
    pub version: String,
    pub schemas: Vec<Schema>,
}

/// A CSDL Schema (namespace + contents).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub namespace: String,
    pub entity_types: Vec<EntityType>,
    pub enum_types: Vec<EnumType>,
    pub actions: Vec<Action>,
    pub functions: Vec<Function>,
    pub entity_containers: Vec<EntityContainer>,
    pub terms: Vec<Term>,
}

/// An OData EntityType.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityType {
    pub name: String,
    pub key_properties: Vec<String>,
    pub properties: Vec<Property>,
    pub navigation_properties: Vec<NavigationProperty>,
    pub annotations: Vec<Annotation>,
    /// Whether this entity type supports the OData `$value` media stream (HasStream="true").
    pub has_stream: bool,
}

/// A scalar property on an EntityType.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Property {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
}

/// A navigation property (relationship to another entity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationProperty {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub contains_target: bool,
    pub referential_constraints: Vec<ReferentialConstraint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferentialConstraint {
    pub property: String,
    pub referenced_property: String,
}

/// An OData EnumType.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumType {
    pub name: String,
    pub members: Vec<EnumMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumMember {
    pub name: String,
    pub value: Option<i64>,
}

/// An OData Action (side-effecting operation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub name: String,
    pub is_bound: bool,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<ReturnType>,
    pub annotations: Vec<Annotation>,
}

/// An OData Function (read-only operation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub is_bound: bool,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<ReturnType>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReturnType {
    pub type_name: String,
    pub nullable: bool,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
}

/// An OData EntityContainer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityContainer {
    pub name: String,
    pub entity_sets: Vec<EntitySet>,
    pub action_imports: Vec<ActionImport>,
    pub function_imports: Vec<FunctionImport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySet {
    pub name: String,
    pub entity_type: String,
    pub navigation_bindings: Vec<NavigationBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationBinding {
    pub path: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionImport {
    pub name: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionImport {
    pub name: String,
    pub function: String,
}

/// A Vocabulary Term definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Term {
    pub name: String,
    pub type_name: String,
    pub applies_to: Option<String>,
    pub description: Option<String>,
}

/// An annotation on an entity type, action, function, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub term: String,
    pub value: AnnotationValue,
}

/// Possible annotation value types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnnotationValue {
    String(String),
    Float(f64),
    Bool(bool),
    Int(i64),
    Collection(Vec<String>),
    Record(HashMap<String, String>),
}

impl CsdlDocument {
    /// Find all schemas matching a namespace prefix.
    pub fn schemas_by_namespace(&self, prefix: &str) -> Vec<&Schema> {
        self.schemas
            .iter()
            .filter(|s| s.namespace.starts_with(prefix))
            .collect()
    }
}

impl Schema {
    /// Find an entity type by name.
    pub fn entity_type(&self, name: &str) -> Option<&EntityType> {
        self.entity_types.iter().find(|e| e.name == name)
    }

    /// Find an action by name.
    pub fn action(&self, name: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.name == name)
    }

    /// Find a function by name.
    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// Find an enum type by name.
    pub fn enum_type(&self, name: &str) -> Option<&EnumType> {
        self.enum_types.iter().find(|e| e.name == name)
    }
}

impl EntityType {
    /// Get an annotation by term name (short or fully qualified).
    pub fn annotation(&self, term: &str) -> Option<&Annotation> {
        self.annotations
            .iter()
            .find(|a| a.term == term || a.term.ends_with(&format!(".{term}")))
    }

    /// Get the state machine states from annotations.
    pub fn state_machine_states(&self) -> Option<Vec<String>> {
        self.annotation("StateMachine.States")
            .and_then(|a| match &a.value {
                AnnotationValue::Collection(v) => Some(v.clone()),
                _ => None,
            })
    }

    /// Get the initial state from annotations.
    pub fn initial_state(&self) -> Option<String> {
        self.annotation("StateMachine.InitialState")
            .and_then(|a| match &a.value {
                AnnotationValue::String(s) => Some(s.clone()),
                _ => None,
            })
    }

    /// Get the TLA+ spec path from annotations.
    pub fn tla_spec_path(&self) -> Option<String> {
        self.annotation("StateMachine.TlaSpec")
            .and_then(|a| match &a.value {
                AnnotationValue::String(s) => Some(s.clone()),
                _ => None,
            })
    }
}

impl Action {
    /// Get an annotation by term name.
    pub fn annotation(&self, term: &str) -> Option<&Annotation> {
        self.annotations
            .iter()
            .find(|a| a.term == term || a.term.ends_with(&format!(".{term}")))
    }

    /// Get valid-from states for this action.
    pub fn valid_from_states(&self) -> Option<Vec<String>> {
        self.annotation("StateMachine.ValidFromStates")
            .and_then(|a| match &a.value {
                AnnotationValue::Collection(v) => Some(v.clone()),
                _ => None,
            })
    }

    /// Get the target state after this action.
    pub fn target_state(&self) -> Option<String> {
        self.annotation("StateMachine.TargetState")
            .and_then(|a| match &a.value {
                AnnotationValue::String(s) => Some(s.clone()),
                _ => None,
            })
    }

    /// Get the binding parameter type (the entity this action is bound to).
    pub fn binding_type(&self) -> Option<&str> {
        if !self.is_bound {
            return None;
        }
        self.parameters.first().map(|p| p.type_name.as_str())
    }
}

#[cfg(test)]
#[path = "types_test.rs"]
mod tests;
