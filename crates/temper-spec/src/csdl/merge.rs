//! Merge two [`CsdlDocument`]s by combining their schemas.

use super::types::{CsdlDocument, EntityContainer, Schema};

/// Merge two CSDL documents by combining their schemas.
///
/// For schemas with matching namespaces, entity types, actions, functions,
/// and entity containers are merged by name (incoming wins on conflict).
/// Schemas in `incoming` that don't exist in `existing` are appended.
pub fn merge_csdl(existing: &CsdlDocument, incoming: &CsdlDocument) -> CsdlDocument {
    let mut result = existing.clone();

    for incoming_schema in &incoming.schemas {
        merge_schema(&mut result.schemas, incoming_schema);
    }

    result
}

fn merge_schema(schemas: &mut Vec<Schema>, incoming_schema: &Schema) {
    let Some(result_schema) = schemas
        .iter_mut()
        .find(|schema| schema.namespace == incoming_schema.namespace)
    else {
        schemas.push(incoming_schema.clone());
        return;
    };

    merge_replace_by_name(
        &mut result_schema.entity_types,
        &incoming_schema.entity_types,
        |item| item.name.as_str(),
    );
    merge_replace_by_name(
        &mut result_schema.enum_types,
        &incoming_schema.enum_types,
        |item| item.name.as_str(),
    );
    merge_replace_by_name(
        &mut result_schema.actions,
        &incoming_schema.actions,
        |item| item.name.as_str(),
    );
    merge_replace_by_name(
        &mut result_schema.functions,
        &incoming_schema.functions,
        |item| item.name.as_str(),
    );

    for container in &incoming_schema.entity_containers {
        merge_entity_container(&mut result_schema.entity_containers, container);
    }

    merge_append_missing_by_name(&mut result_schema.terms, &incoming_schema.terms, |item| {
        item.name.as_str()
    });
}

fn merge_entity_container(containers: &mut Vec<EntityContainer>, incoming: &EntityContainer) {
    let Some(existing) = containers
        .iter_mut()
        .find(|container| container.name == incoming.name)
    else {
        containers.push(incoming.clone());
        return;
    };

    merge_replace_by_name(&mut existing.entity_sets, &incoming.entity_sets, |item| {
        item.name.as_str()
    });
    merge_append_missing_by_name(
        &mut existing.action_imports,
        &incoming.action_imports,
        |item| item.name.as_str(),
    );
    merge_append_missing_by_name(
        &mut existing.function_imports,
        &incoming.function_imports,
        |item| item.name.as_str(),
    );
}

fn merge_replace_by_name<T, F>(target: &mut Vec<T>, incoming: &[T], name: F)
where
    T: Clone,
    F: Fn(&T) -> &str + Copy,
{
    for item in incoming {
        if let Some(position) = target
            .iter()
            .position(|existing| name(existing) == name(item))
        {
            target[position] = item.clone();
        } else {
            target.push(item.clone());
        }
    }
}

fn merge_append_missing_by_name<T, F>(target: &mut Vec<T>, incoming: &[T], name: F)
where
    T: Clone,
    F: Fn(&T) -> &str + Copy,
{
    for item in incoming {
        if !target.iter().any(|existing| name(existing) == name(item)) {
            target.push(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csdl::parse_csdl;

    #[test]
    fn merge_adds_new_entity_type_to_existing_namespace() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Order">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
              <EntityContainer Name="Svc">
                <EntitySet Name="Orders" EntityType="App.Order"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let incoming_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Task">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
                <Property Name="Title" Type="Edm.String"/>
              </EntityType>
              <EntityContainer Name="Svc">
                <EntitySet Name="Tasks" EntityType="App.Task"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let existing = parse_csdl(existing_xml).unwrap();
        let incoming = parse_csdl(incoming_xml).unwrap();
        let merged = merge_csdl(&existing, &incoming);

        assert_eq!(merged.schemas.len(), 1);
        let schema = &merged.schemas[0];
        assert_eq!(schema.entity_types.len(), 2);
        assert!(schema.entity_types.iter().any(|e| e.name == "Order"));
        assert!(schema.entity_types.iter().any(|e| e.name == "Task"));

        let container = &schema.entity_containers[0];
        assert_eq!(container.entity_sets.len(), 2);
        assert!(container.entity_sets.iter().any(|e| e.name == "Orders"));
        assert!(container.entity_sets.iter().any(|e| e.name == "Tasks"));
    }

    #[test]
    fn merge_appends_new_namespace() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Order">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let incoming_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="Custom" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Widget">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let existing = parse_csdl(existing_xml).unwrap();
        let incoming = parse_csdl(incoming_xml).unwrap();
        let merged = merge_csdl(&existing, &incoming);

        assert_eq!(merged.schemas.len(), 2);
        assert!(merged.schemas.iter().any(|s| s.namespace == "App"));
        assert!(merged.schemas.iter().any(|s| s.namespace == "Custom"));
    }

    #[test]
    fn merge_overwrites_existing_entity_type() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Order">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let incoming_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Order">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
                <Property Name="Title" Type="Edm.String"/>
              </EntityType>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let existing = parse_csdl(existing_xml).unwrap();
        let incoming = parse_csdl(incoming_xml).unwrap();
        let merged = merge_csdl(&existing, &incoming);

        let order = merged.schemas[0]
            .entity_types
            .iter()
            .find(|e| e.name == "Order")
            .unwrap();
        // Incoming version has 2 properties (Id + Title), existing had 1 (Id).
        assert_eq!(order.properties.len(), 2);
    }
}
