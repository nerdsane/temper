//! Awaited-integration selection for bound collection deliveries.

use super::super::BoundDelivery;

pub(super) fn await_bound_delivery_integration(bound_delivery: Option<&BoundDelivery>) -> bool {
    bound_delivery.is_some_and(|delivery| {
        delivery.collection.as_ref().is_some_and(|context| {
            context.role == crate::trigger::collection_workflow::CollectionDeliveryRole::Member
        })
    })
}
