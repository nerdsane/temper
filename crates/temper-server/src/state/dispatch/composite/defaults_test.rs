use super::*;

#[tokio::test]
async fn parent_gated_contracted_creation_preserves_defaults_on_both_recovery_paths() {
    let store = SimEventStore::no_faults(467);
    let spec = REF_IOA
        .replace(
            "initial = \"Active\"",
            "initial = \"Active\"\nstrict_action_params = true",
        )
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"revision\"\ntype = \"counter\"\ninitial = 7\n[[action]]",
            1,
        );
    let state = ServerState::with_storage_stack(
        ActorSystem::new("contracted-ref-replay"),
        parse_csdl(COMPOSITE_CSDL).unwrap(),
        COMPOSITE_CSDL.into(),
        BTreeMap::from([("Parent".into(), PARENT_IOA.into()), ("Ref".into(), spec)]),
        StorageStack::from_sim(store.clone(), None),
    )
    .unwrap();
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    state.apply_composite_integration_result(&tenant,"Parent","parent","IngestPack",&json!({"sub_writes":[{
        "entity_type":"Ref","entity_id":"ref","action":"Create","params":{
            "RepositoryId":"repo","Name":"refs/heads/topic","TargetCommitSha":"abc","Kind":"branch"
        }
    }]}),&AgentContext::for_service("composite-test")).await.unwrap();
    let staged = state
        .get_tenant_entity_state(&tenant, "Ref", "ref")
        .await
        .unwrap()
        .state;
    assert_eq!(staged.counters["revision"], 7);
    let events = store.dump_journal("default:Ref:ref");
    let bootstrap = events
        .iter()
        .find(|event| event.event_type == "Created")
        .unwrap();
    assert_eq!(bootstrap.payload["params"], json!({}));
    assert_eq!(
        bootstrap.payload["initial_values"]["counters"],
        json!({"revision":7})
    );
    assert_eq!(
        bootstrap.payload["initial_values"]["fields"],
        json!({"TargetCommitSha":"", "revision":7})
    );
    let create = events
        .iter()
        .find(|event| event.event_type == "Create")
        .unwrap();
    assert_eq!(
        create.payload["params"],
        json!({
            "RepositoryId":"repo", "Name":"refs/heads/topic", "TargetCommitSha":"abc", "Kind":"branch"
        }),
        "bootstrap normalization must not change declared Create action parameters"
    );

    let table = state.transition_table_for_dispatch(&tenant, "Ref").unwrap();
    let journal = crate::storage::BoxedEventStore::new(store);
    let authoritative = crate::entity_actor::recover_authoritative_entity_state_from_store(
        "default",
        "Ref",
        "ref",
        &table,
        &journal,
        BackendLabel::Sim,
        &json!({}),
        None,
    )
    .await
    .unwrap();
    let ordinary = crate::entity_actor::recover_entity_state_from_store(
        "default",
        "Ref",
        "ref",
        &table,
        &journal,
        BackendLabel::Sim,
        &json!({}),
        None,
        false,
    )
    .await
    .unwrap();
    for recovered in [authoritative, ordinary] {
        assert_eq!(recovered.counters.get("revision"), Some(&7));
        assert_eq!(recovered.fields, staged.fields);
    }
}
