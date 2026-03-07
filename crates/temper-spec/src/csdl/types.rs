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
mod tests {
    use super::*;

    fn sample_schema() -> Schema {
        Schema {
            namespace: "TestNs".into(),
            entity_types: vec![
                EntityType {
                    name: "Order".into(),
                    key_properties: vec!["Id".into()],
                    properties: vec![],
                    navigation_properties: vec![],
                    annotations: vec![
                        Annotation {
                            term: "StateMachine.States".into(),
                            value: AnnotationValue::Collection(vec![
                                "Draft".into(),
                                "Active".into(),
                            ]),
                        },
                        Annotation {
                            term: "StateMachine.InitialState".into(),
                            value: AnnotationValue::String("Draft".into()),
                        },
                    ],
                },
                EntityType {
                    name: "Customer".into(),
                    key_properties: vec!["Id".into()],
                    properties: vec![],
                    navigation_properties: vec![],
                    annotations: vec![],
                },
            ],
            enum_types: vec![EnumType {
                name: "Status".into(),
                members: vec![
                    EnumMember {
                        name: "Active".into(),
                        value: Some(0),
                    },
                    EnumMember {
                        name: "Inactive".into(),
                        value: Some(1),
                    },
                ],
            }],
            actions: vec![Action {
                name: "Submit".into(),
                is_bound: true,
                parameters: vec![Parameter {
                    name: "bindingParameter".into(),
                    type_name: "TestNs.Order".into(),
                    nullable: false,
                    default_value: None,
                }],
                return_type: None,
                annotations: vec![
                    Annotation {
                        term: "StateMachine.ValidFromStates".into(),
                        value: AnnotationValue::Collection(vec!["Draft".into()]),
                    },
                    Annotation {
                        term: "StateMachine.TargetState".into(),
                        value: AnnotationValue::String("Active".into()),
                    },
                ],
            }],
            functions: vec![Function {
                name: "GetTotal".into(),
                is_bound: false,
                parameters: vec![],
                return_type: Some(ReturnType {
                    type_name: "Edm.Decimal".into(),
                    nullable: false,
                    precision: Some(10),
                    scale: Some(2),
                }),
                annotations: vec![],
            }],
            entity_containers: vec![],
            terms: vec![],
        }
    }

    #[test]
    fn schema_find_entity_type() {
        let schema = sample_schema();
        assert!(schema.entity_type("Order").is_some());
        assert!(schema.entity_type("Customer").is_some());
        assert!(schema.entity_type("Missing").is_none());
    }

    #[test]
    fn schema_find_action() {
        let schema = sample_schema();
        assert!(schema.action("Submit").is_some());
        assert!(schema.action("Missing").is_none());
    }

    #[test]
    fn schema_find_function() {
        let schema = sample_schema();
        assert!(schema.function("GetTotal").is_some());
        assert!(schema.function("Missing").is_none());
    }

    #[test]
    fn schema_find_enum_type() {
        let schema = sample_schema();
        assert!(schema.enum_type("Status").is_some());
        assert!(schema.enum_type("Missing").is_none());
    }

    #[test]
    fn entity_type_state_machine_states() {
        let schema = sample_schema();
        let order = schema.entity_type("Order").unwrap();
        let states = order.state_machine_states().unwrap();
        assert_eq!(states, vec!["Draft", "Active"]);
    }

    #[test]
    fn entity_type_initial_state() {
        let schema = sample_schema();
        let order = schema.entity_type("Order").unwrap();
        assert_eq!(order.initial_state().unwrap(), "Draft");
    }

    #[test]
    fn entity_type_no_annotations() {
        let schema = sample_schema();
        let customer = schema.entity_type("Customer").unwrap();
        assert!(customer.state_machine_states().is_none());
        assert!(customer.initial_state().is_none());
        assert!(customer.tla_spec_path().is_none());
    }

    #[test]
    fn entity_type_annotation_short_name_match() {
        let schema = sample_schema();
        let order = schema.entity_type("Order").unwrap();
        // annotation() matches both exact and suffix ".{term}"
        assert!(order.annotation("StateMachine.States").is_some());
        assert!(order.annotation("States").is_some());
        assert!(order.annotation("NonExistent").is_none());
    }

    #[test]
    fn action_valid_from_states() {
        let schema = sample_schema();
        let submit = schema.action("Submit").unwrap();
        assert_eq!(submit.valid_from_states().unwrap(), vec!["Draft"]);
    }

    #[test]
    fn action_target_state() {
        let schema = sample_schema();
        let submit = schema.action("Submit").unwrap();
        assert_eq!(submit.target_state().unwrap(), "Active");
    }

    #[test]
    fn action_binding_type_bound() {
        let schema = sample_schema();
        let submit = schema.action("Submit").unwrap();
        assert_eq!(submit.binding_type(), Some("TestNs.Order"));
    }

    #[test]
    fn action_binding_type_unbound() {
        let action = Action {
            name: "Unbound".into(),
            is_bound: false,
            parameters: vec![],
            return_type: None,
            annotations: vec![],
        };
        assert!(action.binding_type().is_none());
    }

    #[test]
    fn csdl_document_schemas_by_namespace() {
        let doc = CsdlDocument {
            version: "4.0".into(),
            schemas: vec![
                sample_schema(),
                Schema {
                    namespace: "OtherNs".into(),
                    entity_types: vec![],
                    enum_types: vec![],
                    actions: vec![],
                    functions: vec![],
                    entity_containers: vec![],
                    terms: vec![],
                },
            ],
        };
        assert_eq!(doc.schemas_by_namespace("Test").len(), 1);
        assert_eq!(doc.schemas_by_namespace("Other").len(), 1);
        assert_eq!(doc.schemas_by_namespace("None").len(), 0);
    }

    #[test]
    fn annotation_value_variants() {
        let s = AnnotationValue::String("hello".into());
        let f = AnnotationValue::Float(3.14);
        let b = AnnotationValue::Bool(true);
        let i = AnnotationValue::Int(42);
        let c = AnnotationValue::Collection(vec!["a".into()]);
        let r = AnnotationValue::Record(HashMap::from([("k".into(), "v".into())]));
        // Verify Debug works (ensures derive is correct)
        for v in [&s, &f, &b, &i, &c, &r] {
            assert!(!format!("{v:?}").is_empty());
        }
    }
}
