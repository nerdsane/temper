//! The specs the simulation runs: a `Job` whose timers a move sets, a `Pass`
//! created straight into a timed state, and the platform's
//! `PrincipalGeneration`, whose counters sign users out and revoke grants.

pub(super) const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Held", "Done", "Taken"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Held", "Done", "Taken"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 600
on_timeout = "Expire"

[[action]]
name = "Hold"
from = ["Idle"]
to = "Held"
effect = ["schedule('Take', 600)"]

[[action]]
name = "Take"
kind = "internal"
from = ["Held"]
to = "Taken"
"#;

pub(super) const PASS: &str = r#"
[automaton]
name = "Pass"
states = ["Open", "Lapsed"]
initial = "Open"
allow_indefinite_states = ["Lapsed"]

[[action]]
name = "Lapse"
kind = "internal"
from = ["Open"]
to = "Lapsed"

[[state_timeout]]
state = "Open"
after_seconds = 600
on_timeout = "Lapse"
"#;

/// The platform's sign-out generation, as the platform installs it.
pub(super) const PRINCIPAL_GENERATION: &str = r#"
[automaton]
name = "PrincipalGeneration"
states = ["Active"]
initial = "Active"

[[state]]
name = "generation"
type = "counter"
initial = 0

[[action]]
name = "BumpGeneration"
kind = "input"
from = ["Active"]
effect = ["generation += 1"]
"#;

pub(super) const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Hold" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Take" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityType Name="Pass"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Lapse" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Pass"/></Action>
<EntityType Name="PrincipalGeneration"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="generation" Type="Edm.Int64"/>
</EntityType>
<Action Name="BumpGeneration" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.PrincipalGeneration"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/><EntitySet Name="Passes" EntityType="Jobs.Pass"/><EntitySet Name="PrincipalGenerations" EntityType="Jobs.PrincipalGeneration"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;
