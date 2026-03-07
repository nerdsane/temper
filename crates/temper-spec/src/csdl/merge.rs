//! Merge two [`CsdlDocument`]s by combining their schemas.

use super::types::CsdlDocument;

/// Merge two CSDL documents by combining their schemas.
///
/// For schemas with matching namespaces, entity types, actions, functions,
/// and entity containers are merged by name (incoming wins on conflict).
/// Schemas in `incoming` that don't exist in `existing` are appended.
pub fn merge_csdl(existing: &CsdlDocument, incoming: &CsdlDocument) -> CsdlDocument {
    let mut result = existing.clone();

    for incoming_schema in &incoming.schemas {
        if let Some(result_schema) = result
            .schemas
            .iter_mut()
            .find(|s| s.namespace == incoming_schema.namespace)
        {
            // Merge entity types by name.
            for et in &incoming_schema.entity_types {
                if let Some(pos) = result_schema
                    .entity_types
                    .iter()
                    .position(|e| e.name == et.name)
                {
                    result_schema.entity_types[pos] = et.clone();
                } else {
                    result_schema.entity_types.push(et.clone());
                }
            }
            // Merge enum types by name.
            for et in &incoming_schema.enum_types {
                if let Some(pos) = result_schema
                    .enum_types
                    .iter()
                    .position(|e| e.name == et.name)
                {
                    result_schema.enum_types[pos] = et.clone();
                } else {
                    result_schema.enum_types.push(et.clone());
                }
            }
            // Merge actions by name.
            for action in &incoming_schema.actions {
                if let Some(pos) = result_schema
                    .actions
                    .iter()
                    .position(|a| a.name == action.name)
                {
                    result_schema.actions[pos] = action.clone();
                } else {
                    result_schema.actions.push(action.clone());
                }
            }
            // Merge functions by name.
            for func in &incoming_schema.functions {
                if let Some(pos) = result_schema
                    .functions
                    .iter()
                    .position(|f| f.name == func.name)
                {
                    result_schema.functions[pos] = func.clone();
                } else {
                    result_schema.functions.push(func.clone());
                }
            }
            // Merge entity containers by name, merging entity sets within.
            for container in &incoming_schema.entity_containers {
                if let Some(existing_container) = result_schema
                    .entity_containers
                    .iter_mut()
                    .find(|c| c.name == container.name)
                {
                    for es in &container.entity_sets {
                        if let Some(pos) = existing_container
                            .entity_sets
                            .iter()
                            .position(|e| e.name == es.name)
                        {
                            existing_container.entity_sets[pos] = es.clone();
                        } else {
                            existing_container.entity_sets.push(es.clone());
                        }
                    }
                    for ai in &container.action_imports {
                        if !existing_container
                            .action_imports
                            .iter()
                            .any(|a| a.name == ai.name)
                        {
                            existing_container.action_imports.push(ai.clone());
                        }
                    }
                    for fi in &container.function_imports {
                        if !existing_container
                            .function_imports
                            .iter()
                            .any(|f| f.name == fi.name)
                        {
                            existing_container.function_imports.push(fi.clone());
                        }
                    }
                } else {
                    result_schema.entity_containers.push(container.clone());
                }
            }
            // Merge terms by name.
            for term in &incoming_schema.terms {
                if !result_schema.terms.iter().any(|t| t.name == term.name) {
                    result_schema.terms.push(term.clone());
                }
            }
        } else {
            // New namespace — append entire schema.
            result.schemas.push(incoming_schema.clone());
        }
    }

    result
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
