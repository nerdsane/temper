use super::parse_automaton;

const COLLECTION_WORKFLOW_IOA: &str = r#"
[automaton]
name = "Batch"
states = ["Idle", "Running", "Done", "Failed", "Cancelled", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done", "Failed", "Cancelled", "TimedOut"]

[[state]]
name = "member_ids"
type = "list"
initial = "[]"

[[action]]
name = "StartChecks"
from = ["Idle"]
to = "Running"

[[action]]
name = "CancelChecks"
from = ["Running"]
to = "Running"

[[action]]
name = "ChecksTimedOut"
from = ["Running"]
to = "Running"

[[action]]
name = "ChecksSucceeded"
from = ["Running"]
to = "Done"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "ChecksPartiallyFailed"
from = ["Running"]
to = "Failed"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "ChecksFailed"
from = ["Running"]
to = "Failed"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "ChecksCancelled"
from = ["Running"]
to = "Cancelled"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[action]]
name = "ChecksTimedOutJoined"
from = ["Running"]
to = "TimedOut"
params = [{ name = "workflow_id", type = "string" }, { name = "total_members", type = "int" }, { name = "succeeded_members", type = "int" }, { name = "failed_members", type = "int" }, { name = "cancelled_members", type = "int" }, { name = "timed_out_members", type = "int" }]

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "ChecksTimedOut"
reset_on = ["StartChecks"]

[[collection_workflow]]
name = "run_checks"
start_action = "StartChecks"
cancel_action = "CancelChecks"
timeout_action = "ChecksTimedOut"
roster_field = "member_ids"
member_entity = "CheckRun"
member_action = "Start"
member_cancel_action = "Cancel"
max_members = 64
max_concurrency = 8
max_attempts = 5
on_success = "ChecksSucceeded"
on_partial_failure = "ChecksPartiallyFailed"
on_failure = "ChecksFailed"
on_cancelled = "ChecksCancelled"
on_timed_out = "ChecksTimedOutJoined"
"#;

#[test]
fn collection_workflow_parses_and_validates_source_contract() {
    let automaton = parse_automaton(COLLECTION_WORKFLOW_IOA).expect("valid collection workflow");
    let workflow = &automaton.collection_workflows[0];
    assert_eq!(workflow.name, "run_checks");
    assert_eq!(workflow.max_members, 64);
    assert_eq!(workflow.member_entity, "CheckRun");
}

#[test]
fn collection_workflow_rejects_budget_and_timeout_drift() {
    let oversized = COLLECTION_WORKFLOW_IOA.replace("max_members = 64", "max_members = 65");
    assert!(parse_automaton(&oversized).is_err());

    let wrong_reset = COLLECTION_WORKFLOW_IOA.replace(
        "reset_on = [\"StartChecks\"]",
        "reset_on = [\"CancelChecks\"]",
    );
    assert!(parse_automaton(&wrong_reset).is_err());
}

#[test]
fn collection_workflow_rejects_duplicate_reserved_parameters() {
    let duplicate = COLLECTION_WORKFLOW_IOA.replacen(
        r#"{ name = "timed_out_members", type = "int" }"#,
        r#"{ name = "workflow_id", type = "string" }"#,
        1,
    );
    let error = parse_automaton(&duplicate).expect_err("duplicate reserved parameter must fail");
    assert!(error.to_string().contains("declared twice"));
}
