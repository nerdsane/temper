//! The DST's workload: its spec, how each record fires, and when a fire
//! is on time.

use chrono::{DateTime, Utc};

/// How long `Start` and every `Tick` wait before their action fires, and how
/// far apart a cron's runs are.
pub(crate) const PERIOD: i64 = 600;

/// A fire after the boot must come at its due time; one that fell due while
/// the process was down, within this long of the boot (eight fire at once).
pub(crate) const BOOT_FIRE_WINDOW: i64 = 5;

/// How late a fire whose writes fail may land: its retries (the store fails
/// up to three writes).
pub(crate) const RETRY_WINDOW: i64 = 15;

/// How long after a boot whose row listing failed the rows are listed again.
pub(crate) const SCAN_RETRY_WAIT: i64 = 1;

/// How often the cron callback looks for a `Run` to answer, so how late it may
/// plan the next one.
pub(crate) const CALLBACK_LAG: i64 = 30;

pub(crate) const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Active", "Planned", "Running", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Waiting", "Active", "Planned", "Running", "Done"]

[[state]]
name = "reports"
type = "counter"
initial = 0

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
effect = ["schedule('Fire', 600)"]

[[action]]
name = "StartAt"
from = ["Idle"]
to = "Waiting"
params = ["due"]
effect = ["schedule_at('Fire', due)"]

[[action]]
name = "Activate"
from = ["Idle"]
to = "Active"
effect = ["schedule('Tick', 600)"]

[[action]]
name = "Tick"
from = ["Active"]
effect = ["schedule('Tick', 600)"]

[[action]]
name = "Fire"
from = ["Waiting", "Done"]
to = "Done"

[[action]]
name = "Plan"
from = ["Idle", "Planned"]
to = "Planned"
params = ["next_run_at"]
effect = ["schedule_at('Run', next_run_at)"]

[[action]]
name = "Run"
from = ["Planned"]

# A run that reports progress and checks on itself at one deadline, as a
# KatagamiAppRun does: launching and every report schedule the same Check.
[[action]]
name = "Launch"
from = ["Idle"]
to = "Running"
params = ["check_at"]
effect = ["schedule_at('Check', check_at)"]

[[action]]
name = "Progress"
from = ["Running"]
params = ["check_at"]
effect = ["reports += 1", "schedule_at('Check', check_at)"]

[[action]]
name = "Finish"
from = ["Running"]
to = "Done"

[[action]]
name = "Check"
from = ["Running"]
"#;

pub(crate) const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="due" Type="Edm.String"/>
<Property Name="next_run_at" Type="Edm.String"/>
<Property Name="check_at" Type="Edm.String"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="StartAt" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="due" Type="Edm.String"/></Action>
<Action Name="Activate" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Plan" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="next_run_at" Type="Edm.String"/></Action>
<Action Name="Run" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Launch" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="check_at" Type="Edm.String"/></Action>
<Action Name="Progress" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="check_at" Type="Edm.String"/></Action>
<Action Name="Finish" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Check" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

pub(crate) const TENANT: &str = "alpha";

/// How a record of the workload fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// `Fire`, once.
    Once,
    /// `Tick`, every [`PERIOD`], each scheduling the next.
    Repeat,
    /// `Run`, at the time `Plan` stored; the callback plans the next.
    Cron,
}

impl Kind {
    pub(crate) fn action(self) -> &'static str {
        match self {
            Kind::Once => "Fire",
            Kind::Repeat => "Tick",
            Kind::Cron => "Run",
        }
    }
}

/// What a record of the workload armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Arm {
    pub(crate) kind: Kind,
    /// When its first fire is due.
    pub(crate) due: DateTime<Utc>,
    /// Whether the store faults its first fire's write.
    pub(crate) faulted: bool,
}

impl Arm {
    /// How late after its due time a fire of this record may land.
    pub(crate) fn late(&self) -> i64 {
        let retries = if self.faulted { RETRY_WINDOW } else { 1 };
        match self.kind {
            Kind::Cron => retries + CALLBACK_LAG,
            Kind::Once | Kind::Repeat => retries,
        }
    }
}

/// When the first process stopped and the second booted.
pub(crate) struct Restart {
    pub(crate) stop: DateTime<Utc>,
    /// When the second process could list its rows: its start, or the
    /// listing's retry when the first one failed.
    pub(crate) boot: DateTime<Utc>,
}

impl Restart {
    /// A fire due at `due` came at `fire` on time: at its due time when that
    /// fell while a process ran, or soon after the boot when it fell while
    /// none did. A due time well before the stop that fired only after the
    /// boot was missed while the first process ran.
    pub(crate) fn on_time(&self, due: DateTime<Utc>, fire: DateTime<Utc>, late: i64) -> bool {
        if fire < self.boot || due > self.boot {
            (-1..=late).contains(&(fire - due).num_seconds())
        } else {
            (due - self.stop).num_seconds() >= -late
                && (fire - self.boot).num_seconds() <= BOOT_FIRE_WINDOW + late
        }
    }
}
