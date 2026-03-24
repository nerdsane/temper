use super::*;

#[test]
fn test_parse_reference_example_csdl() {
    let xml = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
    let doc = parse_csdl(xml).expect("should parse without error");
    let schema = example_schema(&doc);

    assert_eq!(doc.version, "4.0");
    assert_eq!(doc.schemas.len(), 2);
    assert_example_entities(schema);
    assert_example_order(schema);
    assert_example_operations(schema);
    assert_example_container(schema);
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

fn example_schema(doc: &CsdlDocument) -> &Schema {
    doc.schemas
        .iter()
        .find(|schema| schema.namespace == "Temper.Example")
        .expect("should have Temper.Example schema")
}

fn assert_example_entities(schema: &Schema) {
    assert_eq!(schema.entity_types.len(), 7);
    assert!(schema.entity_type("Customer").is_some());
    assert!(schema.entity_type("Order").is_some());
    assert!(schema.entity_type("Product").is_some());
    assert!(schema.entity_type("Payment").is_some());
    assert!(schema.entity_type("Shipment").is_some());
    assert!(schema.entity_type("OrderItem").is_some());
    assert!(schema.entity_type("Address").is_some());
    assert_eq!(schema.enum_types.len(), 3);
}

fn assert_example_order(schema: &Schema) {
    let order = schema.entity_type("Order").unwrap();
    assert_eq!(order.key_properties, vec!["Id"]);
    assert!(order.properties.len() > 10);

    let states = order.state_machine_states().expect("should have states");
    assert_eq!(states.len(), 10);
    assert!(states.contains(&"Draft".to_string()));
    assert!(states.contains(&"Shipped".to_string()));
    assert_eq!(order.initial_state(), Some("Draft".to_string()));
    assert_eq!(order.tla_spec_path(), Some("order.tla".to_string()));

    let customer_nav = order
        .navigation_properties
        .iter()
        .find(|navigation| navigation.name == "Customer")
        .unwrap();
    assert!(!customer_nav.contains_target);
    assert_eq!(customer_nav.referential_constraints.len(), 1);

    let items_nav = order
        .navigation_properties
        .iter()
        .find(|navigation| navigation.name == "Items")
        .unwrap();
    assert!(items_nav.contains_target);
}

fn assert_example_operations(schema: &Schema) {
    let submit = schema.action("SubmitOrder").unwrap();
    assert!(submit.is_bound);
    assert_eq!(submit.valid_from_states(), Some(vec!["Draft".to_string()]));
    assert_eq!(submit.target_state(), Some("Submitted".to_string()));

    let cancel = schema.action("CancelOrder").unwrap();
    let cancel_from = cancel.valid_from_states().unwrap();
    assert_eq!(cancel_from.len(), 3);

    assert!(schema.function("GetOrderTotal").unwrap().is_bound);
    assert!(!schema.function("SearchProducts").unwrap().is_bound);
}

fn assert_example_container(schema: &Schema) {
    let container = &schema.entity_containers[0];
    assert_eq!(container.name, "ExampleService");
    assert_eq!(container.entity_sets.len(), 5);

    let orders_set = container
        .entity_sets
        .iter()
        .find(|entity_set| entity_set.name == "Orders")
        .unwrap();
    assert_eq!(orders_set.entity_type, "Temper.Example.Order");
    assert_eq!(orders_set.navigation_bindings.len(), 3);
}
