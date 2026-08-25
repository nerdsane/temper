//! Closed delivery-to-member failure classification.

pub(super) fn failure_class(
    status: crate::trigger::delivery::ReactionDeliveryStatus,
) -> super::super::CollectionFailureClass {
    use crate::trigger::delivery::ReactionDeliveryStatus;
    match status {
        ReactionDeliveryStatus::Rejected => super::super::CollectionFailureClass::PermanentRejected,
        ReactionDeliveryStatus::DeadLettered => {
            super::super::CollectionFailureClass::AttemptsExhausted
        }
        ReactionDeliveryStatus::Skipped => super::super::CollectionFailureClass::DeliverySkipped,
        ReactionDeliveryStatus::DroppedAllowed => {
            super::super::CollectionFailureClass::UnsupportedDropAllowed
        }
        ReactionDeliveryStatus::Succeeded
        | ReactionDeliveryStatus::Pending
        | ReactionDeliveryStatus::Claimed
        | ReactionDeliveryStatus::Dispatching => {
            super::super::CollectionFailureClass::IdentityCollision
        }
    }
}
