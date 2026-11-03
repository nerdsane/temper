//! The production dispatch path must clear a reason written by a prior action.
use temper_runtime::{ActorSystem, scheduler::install_deterministic_context, tenant::TenantId};
use temper_server::{
    ServerState, StorageStack, registry::SpecRegistry, request_context::AgentContext,
};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SPEC: &str = include_str!("../../../test-fixtures/specs/string_effect.ioa.toml");
const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="StringRecord"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="error" Type="Edm.String"/><Property Name="reason" Type="Edm.String"/>
</EntityType><EntityContainer Name="Container">
<EntitySet Name="StringRecords" EntityType="Test.StringRecord"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

fn server(store: SimEventStore, source: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("StringRecord", source)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("string-effect"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

#[tokio::test]
async fn dispatch_clears_a_string_and_replay_keeps_it_cleared() {
    for seed in 0..20 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server(store.clone(), SPEC);
        let tenant = TenantId::default();
        let id = format!("record-{seed}");
        for (action, params, expected) in [
            (
                "Fail",
                serde_json::json!({"error": "stage failed"}),
                "stage failed",
            ),
            ("Recover", serde_json::json!({}), ""),
        ] {
            let response = state
                .dispatch_tenant_action(
                    &tenant,
                    "StringRecord",
                    &id,
                    action,
                    params,
                    &AgentContext::default(),
                )
                .await
                .unwrap();
            assert!(
                response.success,
                "seed {seed}, {action}: {:?}",
                response.error
            );
            let read = state
                .get_tenant_entity_state(&tenant, "StringRecord", &id)
                .await
                .unwrap();
            assert_eq!(
                read.state.fields["error"], expected,
                "seed {seed}, {action}"
            );
        }
        let read = state
            .get_tenant_entity_state(&tenant, "StringRecord", &id)
            .await
            .unwrap();
        assert_eq!(read.state.fields["reason"], "recovered");
        drop(state);
        let restarted = server(store, SPEC);
        let read = restarted
            .get_tenant_entity_state(&tenant, "StringRecord", &id)
            .await
            .unwrap();
        assert_eq!(read.state.fields["error"], "", "seed {seed}, replay");
        assert_eq!(read.state.fields["reason"], "recovered");
    }
}

#[test]
fn installation_refuses_wrong_or_undeclared_string_targets() {
    for (target, reason) in [
        ("missing", "unknown state variable 'missing'"),
        ("count", "counter"),
    ] {
        let source = SPEC.replace("error = ''", &format!("{target} = ''"));
        let source =
            format!("{source}\n[[state]]\nname = 'count'\ntype = 'counter'\ninitial = 0\n");
        let mut registry = SpecRegistry::new();
        let error = registry
            .try_register_tenant(
                "default",
                parse_csdl(CSDL).unwrap(),
                CSDL.into(),
                &[("StringRecord", source.as_str())],
            )
            .unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }
}

#[tokio::test]
async fn dispatch_and_replay_keep_parameters_over_string_effects() {
    let source = include_str!("../../../test-fixtures/specs/string_effect_param_order.ioa.toml");
    for seed in 0..20 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server(store.clone(), source);
        let tenant = TenantId::default();
        let id = format!("param-order-{seed}");
        for (action, params, status) in [
            ("Write", serde_json::json!({"error": "detail"}), "Written"),
            ("Read", serde_json::json!({}), "Read"),
        ] {
            let response = state
                .dispatch_tenant_action(
                    &tenant,
                    "StringRecord",
                    &id,
                    action,
                    params,
                    &AgentContext::default(),
                )
                .await
                .unwrap();
            assert!(
                response.success,
                "seed {seed}, {action}: {:?}",
                response.error
            );
            let read = state
                .get_tenant_entity_state(&tenant, "StringRecord", &id)
                .await
                .unwrap();
            assert_eq!(
                read.state.fields["error"], "detail",
                "seed {seed}, {action}"
            );
            assert_eq!(read.state.fields["reason"], "last", "seed {seed}, {action}");
            assert_eq!(read.state.status, status, "seed {seed}, {action}");
        }
        drop(state);
        let restarted = server(store, source);
        let read = restarted
            .get_tenant_entity_state(&tenant, "StringRecord", &id)
            .await
            .unwrap();
        assert_eq!(read.state.fields["error"], "detail", "seed {seed}, replay");
        assert_eq!(read.state.fields["reason"], "last", "seed {seed}, replay");
        assert_eq!(read.state.status, "Read", "seed {seed}, replay");
    }
}
