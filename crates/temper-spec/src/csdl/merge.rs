//! Merge two [`CsdlDocument`]s by combining their schemas.

use super::types::{Action, CsdlDocument, EntityContainer, Schema, TargetedAnnotations};

/// Merge two CSDL documents by combining their schemas.
///
/// For schemas with matching namespaces, entity types, actions, functions,
/// and entity containers are merged by name (incoming wins on conflict).
/// Actions additionally match bound/unbound status and binding parameter type.
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
    merge_actions(&mut result_schema.actions, &incoming_schema.actions);
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
    // A schema-level annotation describes the schema (`Temper.Twin` names a
    // twin), so the incoming value for a term is the current one.
    merge_replace_by_name(
        &mut result_schema.annotations,
        &incoming_schema.annotations,
        |item| item.term.as_str(),
    );
    merge_replace_by_key(
        &mut result_schema.targeted_annotations,
        &incoming_schema.targeted_annotations,
        TargetedAnnotations::key,
    );
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

// Same-named actions on different entity types are distinct CSDL operations.
// Non-binding parameters belong to the definition, so incoming updates them.
fn merge_actions(target: &mut Vec<Action>, incoming: &[Action]) {
    for action in incoming {
        if let Some(position) = target.iter().position(|existing| {
            existing.name == action.name
                && existing.is_bound == action.is_bound
                && existing.binding_type() == action.binding_type()
        }) {
            target[position] = action.clone();
            // Remove stale duplicates of this definition so lookup order cannot
            // select an older signature after an update.
            let mut index = 0;
            target.retain(|existing| {
                let keep = index <= position
                    || existing.name != action.name
                    || existing.is_bound != action.is_bound
                    || existing.binding_type() != action.binding_type();
                index += 1;
                keep
            });
        } else {
            target.push(action.clone());
        }
    }
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

/// Replaces every existing item whose key the incoming set carries with the
/// incoming items of that key, keeping the rest. Unlike `merge_replace_by_name`
/// this keeps several incoming items per key: a document may split one
/// target's annotations across blocks, and each block must land.
fn merge_replace_by_key<T, K, F>(target: &mut Vec<T>, incoming: &[T], key: F)
where
    T: Clone,
    K: PartialEq,
    F: Fn(&T) -> K + Copy,
{
    let incoming_keys: Vec<K> = incoming.iter().map(key).collect();
    target.retain(|t| !incoming_keys.contains(&key(t)));
    target.extend(incoming.iter().cloned());
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
    use crate::csdl::{emit_csdl_xml, parse_csdl};

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
    fn merge_carries_schema_annotations_from_incoming() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <Annotation Term="Temper.Twin" String="Old Name"/>
              <Annotation Term="Temper.Keep" String="kept"/>
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
              <Annotation Term="Temper.Twin" String="Deep Sci-Fi"/>
              <EntityType Name="Task">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let existing = parse_csdl(existing_xml).unwrap();
        let incoming = parse_csdl(incoming_xml).unwrap();
        let merged = merge_csdl(&existing, &incoming);

        let schema = &merged.schemas[0];
        assert_eq!(
            schema.annotations.len(),
            2,
            "incoming replaces by term, the rest stays"
        );
        let twin = schema
            .annotations
            .iter()
            .find(|a| a.term == "Temper.Twin")
            .unwrap();
        assert!(
            matches!(&twin.value, crate::csdl::types::AnnotationValue::String(s) if s == "Deep Sci-Fi")
        );
        assert!(schema.annotations.iter().any(|a| a.term == "Temper.Keep"));
    }

    #[test]
    fn merge_replaces_targeted_annotation_blocks_by_target() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <Annotations Target="App.Order/OwnerId"><Annotation Term="Temper.References" String="User"/></Annotations>
              <Annotations Target="App.Order/ProjectId"><Annotation Term="Temper.References" String="Project"/></Annotations>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;
        let incoming_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <Annotations Target="App.Order/OwnerId"><Annotation Term="Temper.References" String="User,Team"/></Annotations>
              <Annotations Target="App.Task/OwnerId"><Annotation Term="Temper.References" String="User"/></Annotations>
              <Annotations Target="App.Order/OwnerId" Qualifier="alt"><Annotation Term="Temper.References" String="Bot"/></Annotations>
              <Annotations Target="App.Task/OwnerId"><Annotation Term="Temper.ReferenceShape" String="json_list"/></Annotations>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;
        let merged = merge_csdl(
            &parse_csdl(existing_xml).unwrap(),
            &parse_csdl(incoming_xml).unwrap(),
        );
        let blocks = &merged.schemas[0].targeted_annotations;
        assert_eq!(
            blocks.len(),
            5,
            "replaced one, kept one, added one, a qualified block on the same target stays distinct, and a target split across two incoming blocks keeps both"
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|b| b.target == "App.Task/OwnerId")
                .count(),
            2
        );
        let alt = blocks
            .iter()
            .find(|b| b.qualifier.as_deref() == Some("alt"))
            .unwrap();
        assert_eq!(alt.target, "App.Order/OwnerId");
        let owner = blocks
            .iter()
            .find(|b| b.target == "App.Order/OwnerId")
            .unwrap();
        assert!(
            matches!(&owner.annotations[0].value, crate::csdl::types::AnnotationValue::String(s) if s == "User,Team")
        );
        assert!(blocks.iter().any(|b| b.target == "App.Order/ProjectId"));
        assert!(blocks.iter().any(|b| b.target == "App.Task/OwnerId"));
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

    #[test]
    fn merge_preserves_entity_nested_bound_actions_from_incoming() {
        let existing_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="Genesis.AgentAnswers" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Answer">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
              <EntityContainer Name="AgentAnswersService">
                <EntitySet Name="Answers" EntityType="Genesis.AgentAnswers.Answer"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let incoming_xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="Genesis.AgentAnswers" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Answer">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
                <Property Name="ConfidenceLevel" Type="Edm.String"/>
                <Action Name="Calibrate" IsBound="true">
                  <Parameter Name="bindingParameter" Type="Genesis.AgentAnswers.Answer"/>
                  <Parameter Name="confidence_level" Type="Edm.String"/>
                </Action>
              </EntityType>
              <EntityContainer Name="AgentAnswersService">
                <EntitySet Name="Answers" EntityType="Genesis.AgentAnswers.Answer"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;

        let existing = parse_csdl(existing_xml).unwrap();
        let incoming = parse_csdl(incoming_xml).unwrap();
        let merged = merge_csdl(&existing, &incoming);
        let schema = &merged.schemas[0];

        assert!(schema.action("Calibrate").is_some());
        assert!(
            schema
                .entity_type("Answer")
                .unwrap()
                .properties
                .iter()
                .any(|property| property.name == "ConfidenceLevel")
        );

        let emitted = emit_csdl_xml(&merged);
        assert!(emitted.contains(r#"<Action Name="Calibrate" IsBound="true">"#));
    }
}
