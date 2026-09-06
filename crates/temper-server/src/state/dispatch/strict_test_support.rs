//! Native dispatch fixtures. All integrations and identities are local test data.
use crate::{ServerState, registry::SpecRegistry};
use temper_runtime::{ActorSystem, tenant::TenantId};

pub(super) const SPEC: &str = r#"
[automaton]
name = "StrictJob"
states = ["Running", "Done", "Failed"]
initial = "Running"
strict_action_params = true
[[state]]
name = "revision"
type = "counter"
initial = "1"
[[state]]
name = "observed"
type = "string"
initial = ""
[[action]]
name = "Complete"
from = ["Running"]
to = "Done"
params = ["observed", "expected_revision"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_revision"
field = "revision"
[[action]]
name = "Rollover"
from = ["Running"]
params = []
effect = [{type="increment", var="revision"}]
[[action]]
name = "Fail"
from = ["Running"]
to = "Failed"
params = ["error"]
[[action]]
name = "Schedule"
from = ["Running"]
params = []
effect = [{type="schedule", action="Poll", delay_seconds=10}]
[[action]]
name = "Poll"
from = ["Running"]
to = "Done"
params = []
"#;

pub(super) fn state() -> ServerState {
    let csdl = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.StrictTest" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="StrictJob"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="StrictJobs" EntityType="Temper.StrictTest.StrictJob"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.into(),
        &[("StrictJob", SPEC)],
    );
    let state = ServerState::from_registry(ActorSystem::new("strict-native-test"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state
}

pub(super) async fn read(state: &ServerState) -> crate::entity_actor::EntityState {
    state
        .get_tenant_entity_state(&TenantId::default(), "StrictJob", "job")
        .await
        .unwrap()
        .state
}

pub(super) fn refused_is_visible(state: &ServerState) -> bool {
    state
        .entity_observe_log
        .lock()
        .unwrap()
        .values()
        .flatten()
        .any(|event| event.event_name == "integration_callback_rejected")
}
