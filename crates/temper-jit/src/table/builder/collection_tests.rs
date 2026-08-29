use super::*;

#[test]
fn collection_workflow_public_activation_is_compiled() {
    let spec = r#"
[automaton]
name = "Batch"
states = ["Idle", "Running", "Done"]
initial = "Idle"

[[state]]
name = "members"
type = "list"
initial = "[]"

[[action]]
name = "Start"
from = ["Idle"]
to = "Running"

[[action]]
name = "Cancel"
from = ["Running"]
to = "Running"

[[action]]
name = "Timeout"
from = ["Running"]
to = "Running"

[[action]]
name = "Joined1"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "Joined2"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "Joined3"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "Joined4"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "Joined5"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "Timeout"
reset_on = ["Start"]

[[collection_workflow]]
name = "work"
start_action = "Start"
cancel_action = "Cancel"
timeout_action = "Timeout"
roster_field = "members"
member_entity = "Member"
member_action = "Run"
member_cancel_action = "Stop"
max_members = 8
max_concurrency = 2
max_attempts = 5
on_success = "Joined1"
on_partial_failure = "Joined2"
on_failure = "Joined3"
on_cancelled = "Joined4"
on_timed_out = "Joined5"
"#;
    let table = TransitionTable::try_from_ioa_source(spec).expect("verified collection compiles");
    assert_eq!(table.collection_workflows.len(), 1);
    let workflow = &table.collection_workflows[0];
    assert_eq!(workflow.name, "work");
    assert_eq!(workflow.roster_field, "members");
    assert_eq!(workflow.max_members, 8);
    assert_eq!(workflow.max_concurrency, 2);
    assert_eq!(workflow.max_attempts, 5);
}
