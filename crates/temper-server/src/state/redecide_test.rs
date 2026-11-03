//! Tests for a write's decisions and their deadline.

use super::*;

#[test]
fn a_write_on_an_unchanged_record_is_not_decided_again() {
    let mut redecide = Redecide::until(sim_now(), Duration::from_secs(5));
    assert!(!redecide.again(false, "test"));
    assert!(redecide.is_first());
}

#[test]
fn a_write_on_a_changed_record_is_decided_again_until_its_deadline() {
    let mut redecide = Redecide::until(sim_now(), Duration::from_secs(5));
    assert!(redecide.again(true, "test"));
    assert!(!redecide.is_first());

    let mut late = Redecide::until(sim_now(), Duration::ZERO);
    assert!(!late.again(true, "test"));
}
