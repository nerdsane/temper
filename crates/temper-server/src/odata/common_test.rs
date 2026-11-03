//! Tests for the answer to a record that could not be read back for its decision.

use super::*;
use temper_runtime::actor::ActorError;

#[test]
fn an_ask_that_timed_out_is_a_failed_read_not_a_changed_record() {
    // What `get_tenant_entity_state`, and so the read-back, returns when
    // the record's actor does not answer in time.
    let timed_out = DispatchError::Internal(format!(
        "Actor query failed: {}",
        ActorError::AskTimeout(std::time::Duration::from_secs(5))
    ));
    let answer = authorized_snapshot_failure(&timed_out);
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(answer.body["error"]["code"], "ReadError", "{}", answer.body);
}

#[test]
fn a_record_created_differently_is_a_changed_record() {
    let conflict = DispatchError::Conflict(
        "Entity authorization state changed; retry against current state".into(),
    );
    let answer = authorized_snapshot_failure(&conflict);
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(answer.body["error"]["code"], "AuthorizationStateChanged");
}
