use super::await_bound_delivery_integration;
use crate::trigger::collection_workflow::{
    CollectionDeliveryActions, CollectionDeliveryContext, CollectionDeliveryRole,
};
use crate::trigger::dispatcher::BoundDelivery;

fn bound(role: CollectionDeliveryRole) -> BoundDelivery {
    BoundDelivery {
        delivery_id: "delivery".to_string(),
        root_delivery_id: "root".to_string(),
        fencing_token: 1,
        target_entity_id: Some("target".to_string()),
        expected_target_sequence: None,
        state_timeout_state: None,
        source_stream_descriptor: None,
        collection: Some(CollectionDeliveryContext {
            workflow_id: "workflow".to_string(),
            member_id: Some("member".to_string()),
            control_epoch: 0,
            attempts: 1,
            max_attempts: 5,
            execution_deadline: Some(
                temper_runtime::scheduler::sim_now() + chrono::Duration::minutes(1),
            ),
            role,
            terminal_classification: None,
            actions: CollectionDeliveryActions {
                member_entity: "Member".to_string(),
                member_action: "Start".to_string(),
                member_cancel_action: "Cancel".to_string(),
                timeout_action: "Timeout".to_string(),
                on_success: "Succeeded".to_string(),
                on_partial_failure: "PartiallyFailed".to_string(),
                on_failure: "Failed".to_string(),
                on_cancelled: "Cancelled".to_string(),
                on_timed_out: "TimedOut".to_string(),
            },
        }),
    }
}

#[test]
fn only_collection_member_delivery_awaits_its_integration() {
    assert!(await_bound_delivery_integration(Some(&bound(
        CollectionDeliveryRole::Member
    ))));
    assert!(!await_bound_delivery_integration(Some(&bound(
        CollectionDeliveryRole::Cancellation
    ))));
    assert!(!await_bound_delivery_integration(Some(&bound(
        CollectionDeliveryRole::Join
    ))));
    assert!(!await_bound_delivery_integration(None));
}
