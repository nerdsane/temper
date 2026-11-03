//! The specs: a chain that spawns a timed record, a timed action that binds its
//! caller, and the line's Rollout, with and without an integration.

/// Answers `{"seen": ...}`.
pub(super) const ECHO_WASM: &[u8] =
    include_bytes!("../../../temper-wasm/tests/fixtures/trigger_principal_echo.wasm");

/// `Merge` (someone's request) sets off a reaction, `RollOut`, whose effect
/// spawns the Shipment. The Shipment is made and moved only by that chain.
pub(super) const LINE_IOA: &str = r#"
[automaton]
name = "Line"
states = ["Open", "Merging", "Shipping"]
initial = "Open"
allow_indefinite_states = ["Open", "Merging", "Shipping"]
strict_action_params = true

[[state]]
name = "rollout"
type = "string"
initial = ""

[[action]]
name = "Tidy"
kind = "input"
from = ["Open"]
params = []

[[action]]
name = "Merge"
kind = "input"
from = ["Open"]
to = "Merging"
params = []

[[action.triggers]]
name = "merged"
kind = "entity"
target_entity = "Line"
target_action = "RollOut"
resolve_target = { kind = "same_id" }

[[action]]
name = "RollOut"
kind = "input"
from = ["Merging"]
to = "Shipping"
params = []
effect = ["spawn('Shipment', 'Start', rollout)"]
"#;

/// `Start` enters `Staging`, whose timer fires `Diagnose`, which binds whoever
/// it runs as.
pub(super) const SHIPMENT_IOA: &str = r#"
[automaton]
name = "Shipment"
states = ["Idle", "Staging", "Diagnosed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Diagnosed"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Staging"
params = ["parent_type", "parent_id", "line_id"]

[[action]]
name = "Nudge"
kind = "input"
from = ["Staging"]
params = []

[[state_timeout]]
state = "Staging"
after_seconds = AFTER
on_timeout = "Diagnose"

[[action]]
name = "Diagnose"
kind = "internal"
from = ["Staging"]
to = "Diagnosed"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// A timed action that binds its caller. `Nudge` is a move in the timed state
/// that changes nothing else.
pub(super) const CHECK_IOA: &str = r#"
[automaton]
name = "Check"
states = ["Idle", "Checking", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Checking"
params = []

[[action]]
name = "Nudge"
kind = "input"
from = ["Checking"]
params = []

[[state_timeout]]
state = "Checking"
after_seconds = 1
on_timeout = "Finish"

[[action]]
name = "Finish"
kind = "internal"
from = ["Checking"]
to = "Done"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// The line's Rollout: a self-resetting timed `Poll` binds no one and lands,
/// and a reaction to it, `Diagnose`, binds its caller.
pub(super) const POLLER_IOA: &str = r#"
[automaton]
name = "Poller"
states = ["Idle", "Polling", "Stuck"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Stuck"]
strict_action_params = true

[[state]]
name = "polls"
type = "counter"
initial = 0

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Polling"
params = []

[[action]]
name = "Nudge"
kind = "input"
from = ["Polling"]
params = []

[[state_timeout]]
state = "Polling"
after_seconds = 1
on_timeout = "Poll"
reset_on = ["Poll"]

[[action]]
name = "Poll"
kind = "internal"
from = ["Polling"]
params = []
effect = ["polls += 1"]

[[action.triggers]]
name = "diagnose"
kind = "entity"
target_entity = "Poller"
target_action = "Diagnose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Diagnose"
kind = "input"
from = ["Polling"]
to = "Stuck"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// The same, with the refusal after an integration's answer, so well after the
/// timer's fire returned and its next arm was set.
pub(super) const ASKER_IOA: &str = r#"
[automaton]
name = "Asker"
states = ["Idle", "Polling", "Failed", "Stuck"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Failed", "Stuck"]
strict_action_params = true

[[state]]
name = "polls"
type = "counter"
initial = 0

[[state]]
name = "seen"
type = "string"
initial = ""

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Polling"
params = []

[[state_timeout]]
state = "Polling"
after_seconds = 1
on_timeout = "Poll"
reset_on = ["Poll"]

[[action]]
name = "Poll"
kind = "internal"
from = ["Polling"]
params = []
effect = ["polls += 1"]

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[action]]
name = "Saw"
kind = "input"
from = ["Polling"]
params = ["seen"]

[[action.triggers]]
name = "diagnose"
kind = "entity"
target_entity = "Asker"
target_action = "Diagnose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Fail"
kind = "input"
from = ["Polling"]
to = "Failed"
params = []

[[action]]
name = "Diagnose"
kind = "input"
from = ["Polling"]
to = "Stuck"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// `Ask`, a timed action that fires once, moves the record on to Waiting,
/// whose own timer `Tick` needs no caller, and asks an integration. The
/// answer's reaction, `Diagnose`, binds its caller and is refused, after the
/// record left the timer's state.
pub(super) const HOPPER_IOA: &str = r#"
[automaton]
name = "Hopper"
states = ["Idle", "Asking", "Waiting", "Failed", "Stuck"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Failed", "Stuck"]
strict_action_params = true

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[state]]
name = "seen"
type = "string"
initial = ""

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Asking"
params = []

[[state_timeout]]
state = "Asking"
after_seconds = 1
on_timeout = "Ask"

[[action]]
name = "Ask"
kind = "internal"
from = ["Asking"]
to = "Waiting"
params = []

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Tick"
reset_on = ["Tick"]

[[action]]
name = "Tick"
kind = "internal"
from = ["Waiting"]
params = []
effect = ["ticks += 1"]

[[action]]
name = "Saw"
kind = "input"
from = ["Waiting"]
params = ["seen"]

[[action.triggers]]
name = "diagnose"
kind = "entity"
target_entity = "Hopper"
target_action = "Diagnose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Fail"
kind = "input"
from = ["Waiting"]
to = "Failed"
params = []

[[action]]
name = "Diagnose"
kind = "input"
from = ["Waiting"]
to = "Stuck"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

pub(super) const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Needs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Line">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="rollout" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Shipment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="parent_type" Type="Edm.String"/>
        <Property Name="parent_id" Type="Edm.String"/>
        <Property Name="line_id" Type="Edm.String"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Check">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Poller">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="polls" Type="Edm.Int64"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Asker">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="polls" Type="Edm.Int64"/>
        <Property Name="seen" Type="Edm.String"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Hopper">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="ticks" Type="Edm.Int64"/>
        <Property Name="seen" Type="Edm.String"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Lines" EntityType="Temper.Needs.Line"/>
        <EntitySet Name="Shipments" EntityType="Temper.Needs.Shipment"/>
        <EntitySet Name="Checks" EntityType="Temper.Needs.Check"/>
        <EntitySet Name="Pollers" EntityType="Temper.Needs.Poller"/>
        <EntitySet Name="Askers" EntityType="Temper.Needs.Asker"/>
        <EntitySet Name="Hoppers" EntityType="Temper.Needs.Hopper"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
