//! The `Job` specs the state-timer workloads install, and how to install one.

use temper_server::ServerState;
use temper_spec::csdl::parse_csdl;

/// The first spec: `Start` enters `Waiting`. `Expire` and `Approve` are
/// accepted from every live state, so a timer that fired outside its state
/// would leave an event saying so instead of being refused. `Poke` counts
/// itself, so it moves the journal: a machine's move that changes nothing
/// writes no event.
pub(super) const JOB_V1: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Review", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]

[[state]]
name = "pokes"
type = "counter"
initial = 0

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Poke"
from = ["Waiting", "Review"]
effect = ["pokes += 1"]

[[action]]
name = "Expire"
from = ["Waiting", "Review", "Done"]
to = "Done"

[[action]]
name = "Approve"
from = ["Waiting", "Review", "Done"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 3600
on_timeout = "Expire"

[[state_timeout]]
state = "Review"
after_seconds = 1800
on_timeout = "Approve"
"#;

pub(super) const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Approve" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// `Job` with no timeout: records wait in `Waiting` and `Review` for good.
pub(super) fn job_untimed() -> String {
    let untimed = JOB_V1[..JOB_V1.find("[[state_timeout]]").unwrap()].to_string();
    untimed.replacen(
        "allow_indefinite_states = [\"Idle\", \"Done\"]",
        "allow_indefinite_states = [\"Idle\", \"Waiting\", \"Review\", \"Done\"]",
        1,
    )
}

/// The second spec: a new `Start` enters `Review`, and `Waiting`'s timer runs
/// 2700s, not 3600s. A record that already started keeps its stored `Waiting`
/// on replay, so its state now declares another timer than its row names.
pub(super) fn job_v2() -> String {
    let v2 = JOB_V1
        .replacen(
            "from = [\"Idle\"]\nto = \"Waiting\"",
            "from = [\"Idle\"]\nto = \"Review\"",
            1,
        )
        .replacen(
            "after_seconds = 3600\non_timeout = \"Expire\"",
            "after_seconds = 2700\non_timeout = \"Expire\"",
            1,
        );
    assert!(v2.contains("to = \"Review\"") && v2.contains("after_seconds = 2700"));
    v2
}

/// Install `spec` as `tenant`'s `Job`, replacing the one it had.
pub(super) fn install(state: &ServerState, tenant: &str, spec: &str) {
    let mut registry = state.registry.write().unwrap();
    let csdl = parse_csdl(CSDL).unwrap();
    registry.register_tenant(tenant, csdl, CSDL.to_string(), &[("Job", spec)]);
}

/// The declared timer of each state: the action it fires.
pub(super) fn timer_action(state: &str) -> Option<&'static str> {
    match state {
        "Waiting" => Some("Expire"),
        "Review" => Some("Approve"),
        _ => None,
    }
}
