use super::*;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

fn composite_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Repository"
version = "1.0.0"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["PackBytes", "RefUpdates", "ClientRequestId"]
effect = [{ type = "trigger", name = "scm_ingest_pack" }]

[[action.sub_writes]]
target_entity = "Commit"
action = "Create"

[[integration]]
name = "scm_ingest_pack"
trigger = "scm_ingest_pack"
type = "wasm"
module = "scm_ingest_pack"
"#,
    )))
}

fn timed_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "TimedTicket"
states = ["Open", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
reset_on = ["Heartbeat"]
"#,
    )))
}

#[path = "actor_test/lifecycle.rs"]
mod lifecycle;
#[path = "actor_test/persistence.rs"]
mod persistence;
#[path = "actor_test/replay.rs"]
mod replay;
