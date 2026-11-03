//! The specs the owed-writes DST runs, and the module that answers `Ask`.
//!
//! Each move of a `Job` sets off one write the kernel owes: `Ask` a module's
//! `Answer`, `Hold` the `Expire` its timer fires, `Note` a `Count` on the
//! record's `Ledger` through a reaction, and `Spawn` a `Child` and its
//! `Init`. Every owed move is also allowed from the state it leads to, so a
//! write applied twice lands twice and shows in the journal, rather than the
//! second being refused.

/// The `Waiting` timeout in [`JOB`], in seconds.
pub const TIMER_SECS: u64 = 600;

pub const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Asked", "Answered", "Waiting", "Expired", "Noted", "Spawned"]
initial = "Idle"

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "oracle"
kind = "wasm"
module = "oracle"
on_success = "Answer"

[[action]]
name = "Answer"
kind = "input"
from = ["Asked", "Answered"]
to = "Answered"
params = ["value"]

[[action]]
name = "Hold"
kind = "input"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting", "Expired"]
to = "Expired"

[[state_timeout]]
state = "Waiting"
after_seconds = 600
on_timeout = "Expire"

[[action]]
name = "Note"
kind = "input"
from = ["Idle"]
to = "Noted"

[[action.triggers]]
name = "note_counts_on_ledger"
kind = "entity"
target_entity = "Ledger"
target_action = "Count"
resolve_target = { kind = "same_id" }

[[action]]
name = "Spawn"
kind = "input"
from = ["Idle"]
to = "Spawned"
params = ["child_id"]
effect = ["spawn('Child', 'Init', child_ref, params.child_id)"]
"#;

pub const LEDGER: &str = r#"
[automaton]
name = "Ledger"
states = ["Open"]
initial = "Open"

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Count"
kind = "internal"
from = ["Open"]
effect = ["notes += 1"]
"#;

pub const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"

[[action]]
name = "Init"
kind = "input"
from = ["New", "Initialized"]
to = "Initialized"
"#;

pub const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Ledger"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Child"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Jobs" EntityType="Jobs.Job"/><EntitySet Name="Ledgers" EntityType="Jobs.Ledger"/><EntitySet Name="Children" EntityType="Jobs.Child"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A WASM module that answers `{"params": {"value": <value>}}` whatever it
/// is asked: the trigger's `on_success` sends it as `Answer`.
pub fn oracle_module(value: &serde_json::Value) -> String {
    let payload = serde_json::json!({ "params": { "value": value } }).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    // Enough 64 KiB pages to hold the answer.
    let pages = payload.len() / 65_536 + 1;
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") {pages})
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    )
}
