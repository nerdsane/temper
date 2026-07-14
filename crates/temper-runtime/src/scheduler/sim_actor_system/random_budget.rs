//! Random-driver action reservation accounting.

use std::collections::BTreeMap;

pub(super) fn reserve(in_flight_actions: &mut BTreeMap<String, usize>, actor_id: &str) {
    let in_flight = in_flight_actions
        .get_mut(actor_id)
        .expect("registered reservation target");
    *in_flight += 1;
}

pub(super) fn release(
    in_flight_actions: &mut BTreeMap<String, usize>,
    actor_id: &str,
    outcome: &str,
) {
    let in_flight = in_flight_actions
        .get_mut(actor_id)
        .expect("registered reservation target");
    assert!(*in_flight > 0, "{outcome} action must own a reservation");
    *in_flight -= 1;
}
