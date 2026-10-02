use std::collections::BTreeSet;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};

fn document(actions: &str) -> CsdlDocument {
    parse_csdl(&format!(r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="App" xmlns="http://docs.oasis-open.org/odata/ns/edm">{actions}</Schema></edmx:DataServices></edmx:Edmx>"#)).unwrap()
}

fn identities(doc: &CsdlDocument) -> BTreeSet<(String, String, bool, Option<String>)> {
    doc.schemas
        .iter()
        .flat_map(|s| {
            s.actions.iter().map(|a| {
                (
                    s.namespace.clone(),
                    a.name.clone(),
                    a.is_bound,
                    a.binding_type().map(str::to_owned),
                )
            })
        })
        .collect()
}

#[test]
fn same_named_actions_preserve_distinct_bindings_and_replace_same_binding() {
    let before = document(
        r#"
      <Action Name="Prepared" IsBound="true"><Parameter Name="binding" Type="App.LearningRun"/></Action>
      <Action Name="Record" IsBound="true"><Parameter Name="binding" Type="App.Observation"/></Action>
      <Action Name="Prepared" IsBound="false"><Parameter Name="payload" Type="Edm.String"/></Action>
      <Action Name="Prepared" IsBound="true"><Parameter Name="binding" Type="Collection(App.LearningRun)"/></Action>
    "#,
    );
    let incoming = document(
        r#"
      <Action Name="Prepared" IsBound="true"><Parameter Name="binding" Type="App.SemanticRun"/></Action>
      <Action Name="Record" IsBound="true"><Parameter Name="binding" Type="App.Other"/></Action>
      <Action Name="Prepared" IsBound="true"><Parameter Name="renamed_binding" Type="App.LearningRun"/><Parameter Name="new_payload" Type="Edm.String"/></Action>
    "#,
    );
    let merged = merge_csdl(&before, &incoming);
    let expected: BTreeSet<_> = identities(&before)
        .union(&identities(&incoming))
        .cloned()
        .collect();
    assert_eq!(identities(&merged), expected);
    assert_eq!(merged.schemas[0].actions.len(), 6);
    let updated = merged.schemas[0]
        .actions
        .iter()
        .find(|a| a.binding_type() == Some("App.LearningRun"))
        .unwrap();
    assert_eq!(updated.parameters.len(), 2);
    assert_eq!(updated.parameters[0].name, "renamed_binding");
    assert_eq!(
        identities(&parse_csdl(&emit_csdl_xml(&merged)).unwrap()),
        expected
    );
    assert_eq!(
        serde_json::to_value(merge_csdl(&merged, &incoming)).unwrap(),
        serde_json::to_value(&merged).unwrap()
    );
}

#[test]
#[ignore = "requires explicitly supplied generated acceptance metadata fixtures"]
fn captured_metadata_updates_preserve_all_action_bindings() {
    let dir = std::env::var("TEMPER_ACTION_MERGE_FIXTURES").unwrap();
    let before = parse_csdl(
        &std::fs::read_to_string(format!("{dir}/live-metadata-before-options.xml")).unwrap(),
    )
    .unwrap();
    for name in [
        "model-options-native-spec-payload-scoped.json",
        "model-options-native-spec-payload.json",
    ] {
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap())
                .unwrap();
        let incoming = parse_csdl(payload["model.csdl.xml"].as_str().unwrap()).unwrap();
        let merged = merge_csdl(&before, &incoming);
        let roundtrip = parse_csdl(&emit_csdl_xml(&merged)).unwrap();
        let expected: BTreeSet<_> = identities(&before)
            .union(&identities(&incoming))
            .cloned()
            .collect();
        assert_eq!(
            identities(&roundtrip),
            expected,
            "{name} lost action bindings"
        );
        assert_eq!(
            serde_json::to_value(merge_csdl(&roundtrip, &incoming)).unwrap(),
            serde_json::to_value(&roundtrip).unwrap(),
            "repeated merge changed {name}"
        );
        for schema in &incoming.schemas {
            let merged_schema = roundtrip
                .schemas
                .iter()
                .find(|s| s.namespace == schema.namespace)
                .unwrap();
            for action in &schema.actions {
                let matches: Vec<_> = merged_schema
                    .actions
                    .iter()
                    .filter(|a| {
                        a.name == action.name
                            && a.is_bound == action.is_bound
                            && a.binding_type() == action.binding_type()
                    })
                    .collect();
                assert_eq!(matches.len(), 1, "updated action retains duplicates");
                assert_eq!(
                    serde_json::to_value(matches[0]).unwrap(),
                    serde_json::to_value(action).unwrap()
                );
            }
        }
        for schema in &before.schemas {
            let new_schema = roundtrip
                .schemas
                .iter()
                .find(|s| s.namespace == schema.namespace)
                .unwrap();
            for action in &schema.actions {
                let is_updated = incoming
                    .schemas
                    .iter()
                    .filter(|s| s.namespace == schema.namespace)
                    .flat_map(|s| &s.actions)
                    .any(|a| {
                        a.name == action.name
                            && a.is_bound == action.is_bound
                            && a.binding_type() == action.binding_type()
                    });
                if !is_updated {
                    assert!(
                        new_schema
                            .actions
                            .iter()
                            .any(|a| serde_json::to_value(a).unwrap()
                                == serde_json::to_value(action).unwrap()),
                        "unrelated action changed: {} {:?}",
                        action.name,
                        action.binding_type()
                    );
                }
            }
        }
        eprintln!(
            "{name}: preserved {} original identities; {} merged identities",
            identities(&before).len(),
            expected.len()
        );
    }
}

#[test]
fn updating_a_binding_removes_only_its_stale_duplicates() {
    let before = document(
        r#"
      <Action Name="Configure" IsBound="true"><Parameter Name="binding" Type="App.World"/><Parameter Name="old" Type="Edm.String"/></Action>
      <Action Name="Configure" IsBound="true"><Parameter Name="binding" Type="App.Session"/></Action>
      <Action Name="Configure" IsBound="true"><Parameter Name="binding" Type="App.World"/><Parameter Name="stale" Type="Edm.String"/></Action>
    "#,
    );
    let incoming = document(
        r#"<Action Name="Configure" IsBound="true"><Parameter Name="binding" Type="App.World"/><Parameter Name="options" Type="Edm.String"/></Action>"#,
    );
    let merged = merge_csdl(&before, &incoming);
    assert_eq!(merged.schemas[0].actions.len(), 2);
    assert_eq!(merged.schemas[0].actions[0].parameters[1].name, "options");
    assert_eq!(
        serde_json::to_value(&merged.schemas[0].actions[1]).unwrap(),
        serde_json::to_value(&before.schemas[0].actions[1]).unwrap()
    );
    assert_eq!(
        serde_json::to_value(merge_csdl(&merged, &incoming)).unwrap(),
        serde_json::to_value(&merged).unwrap()
    );
}
