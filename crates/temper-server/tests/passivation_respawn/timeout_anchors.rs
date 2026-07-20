//! Timeout-anchor persistence and legacy snapshot upgrade regressions.

use super::common;
use std::collections::BTreeMap;
use temper_runtime::ActorSystem;
use temper_runtime::actor::SystemSignal;
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const INITIAL_UNTIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["Running", "TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"
"#;

const INITIAL_TIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 600
on_timeout = "TimeoutFail"
"#;

pub(super) const TIMED_TASK_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Idle", "Running", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "TimedOut"]

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[path = "timeout_anchors/hotswap.rs"]
mod hotswap;
#[path = "timeout_anchors/snapshot.rs"]
mod snapshot;
#[path = "timeout_anchors/startup.rs"]
mod startup;
