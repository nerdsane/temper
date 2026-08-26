use super::helpers::{
    collection_control_skip_reason, is_expected_target_drop, is_transient_delivery_failure,
};

#[test]
fn retry_classification_uses_closed_source_facts() {
    use crate::trigger::ReactionFailureKind;

    assert!(is_transient_delivery_failure(
        ReactionFailureKind::MailboxCapacityExhausted
    ));
    assert!(is_transient_delivery_failure(
        ReactionFailureKind::AcknowledgementLost
    ));
    assert!(!is_transient_delivery_failure(
        ReactionFailureKind::DispatchConflict
    ));
}

#[test]
fn drop_ok_only_classifies_target_state_mismatch() {
    assert!(is_expected_target_drop(
        "Action 'Capture' not valid from state 'Pending'"
    ));
    assert!(is_expected_target_drop(
        "Action 'Capture' blocked from state 'Pending': guard failed"
    ));
    assert!(!is_expected_target_drop("authorization denied"));
    assert!(!is_expected_target_drop("invalid persisted authority"));
}

#[test]
fn post_control_descendant_fence_has_stable_skip_reason() {
    use crate::trigger::collection_workflow::CollectionDeliveryRole;

    assert_eq!(
        collection_control_skip_reason(
            Some(CollectionDeliveryRole::MemberDescendant),
            "stale collection control epoch at target commit",
        ),
        Some("CollectionControlBeforeDescendantCommit")
    );
    assert_eq!(
        collection_control_skip_reason(
            Some(CollectionDeliveryRole::Member),
            "collection member descendant was fenced by lifecycle change",
        ),
        None
    );
}
