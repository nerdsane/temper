//! The specs the commit-outcome DST runs: a `Tally` whose `Hold` enters a
//! timed state, schedules a `Note` and counts itself on a `Ledger` through
//! a reaction.

pub const TALLY: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Busy"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[key]]
name = "code"
properties = ["Code"]

[[state]]
name = "bumps"
type = "counter"
initial = 0

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Open", "Busy"]
params = ["Mark"]
effect = ["bumps += 1"]

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Busy"
params = ["Mark"]
effect = ["schedule('Note', 3600)"]

[[action.triggers]]
name = "hold_counts_on_ledger"
kind = "entity"
target_entity = "Ledger"
target_action = "Count"
resolve_target = { kind = "same_id" }

[[action]]
name = "Note"
kind = "internal"
from = ["Open", "Busy"]
effect = ["notes += 1"]

[[action]]
name = "Release"
kind = "input"
from = ["Busy"]
to = "Open"
params = ["Mark"]

[[action]]
name = "Expire"
kind = "internal"
from = ["Busy"]
to = "Open"

[[state_timeout]]
state = "Busy"
after_seconds = 86400
on_timeout = "Expire"
"#;

pub const LEDGER: &str = r#"
[automaton]
name = "Ledger"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[state]]
name = "holds"
type = "counter"
initial = 0

[[action]]
name = "Count"
kind = "internal"
from = ["Open"]
effect = ["holds += 1"]
"#;

pub const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Tallies" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="Code" Type="Edm.String"/>
<Property Name="Mark" Type="Edm.String"/>
</EntityType>
<EntityType Name="Ledger"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Tallies" EntityType="Tallies.Tally"/><EntitySet Name="Ledgers" EntityType="Tallies.Ledger"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;
