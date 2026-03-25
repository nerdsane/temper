use super::*;

fn sample_schema() -> Schema {
    Schema {
        namespace: "TestNs".into(),
        entity_types: sample_entity_types(),
        enum_types: sample_enum_types(),
        actions: sample_actions(),
        functions: sample_functions(),
        entity_containers: vec![],
        terms: vec![],
    }
}

fn sample_entity_types() -> Vec<EntityType> {
    vec![
        EntityType {
            name: "Order".into(),
            key_properties: vec!["Id".into()],
            properties: vec![],
            navigation_properties: vec![],
            annotations: vec![
                Annotation {
                    term: "StateMachine.States".into(),
                    value: AnnotationValue::Collection(vec!["Draft".into(), "Active".into()]),
                },
                Annotation {
                    term: "StateMachine.InitialState".into(),
                    value: AnnotationValue::String("Draft".into()),
                },
            ],
            has_stream: false,
        },
        EntityType {
            name: "Customer".into(),
            key_properties: vec!["Id".into()],
            properties: vec![],
            navigation_properties: vec![],
            annotations: vec![],
            has_stream: false,
        },
    ]
}

fn sample_enum_types() -> Vec<EnumType> {
    vec![EnumType {
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
    }]
}

fn sample_actions() -> Vec<Action> {
    vec![Action {
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
    }]
}

fn sample_functions() -> Vec<Function> {
    vec![Function {
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
    }]
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
    let f = AnnotationValue::Float(2.72);
    let b = AnnotationValue::Bool(true);
    let i = AnnotationValue::Int(42);
    let c = AnnotationValue::Collection(vec!["a".into()]);
    let r = AnnotationValue::Record(HashMap::from([("k".into(), "v".into())]));
    for value in [&s, &f, &b, &i, &c, &r] {
        assert!(!format!("{value:?}").is_empty());
    }
}
