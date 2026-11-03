//! Which failed tries of an owed write are kept to try again, and which are
//! final.

use std::time::Duration;

use temper_runtime::actor::ActorError;

use super::{Attempt, DispatchError};

fn kept(error: DispatchError) -> bool {
    matches!(Attempt::of(&Err(error)), Attempt::StoreFailed(_))
}

#[test]
fn a_try_the_store_did_not_take_is_kept() {
    assert!(kept(DispatchError::StoreUnavailable(
        "the store takes no writes".into()
    )));
}

/// A try whose ask timed out is still queued at the actor and may land: the
/// next try, under the same key, is answered as a duplicate if it did.
#[test]
fn a_try_its_actor_did_not_answer_in_time_is_kept() {
    let timed_out = ActorError::AskTimeout(Duration::from_secs(5));
    assert!(kept(DispatchError::from_actor_error(timed_out, 3)));
}

/// An actor whose starts all failed, as they do while its journal cannot be
/// read, stops without handling the move; the next try starts it again.
#[test]
fn a_try_whose_actor_stopped_before_answering_is_kept() {
    assert!(kept(DispatchError::from_actor_error(
        ActorError::Stopped,
        1
    )));
}

#[test]
fn a_full_mailbox_a_refusal_and_a_conflict_are_final() {
    assert!(!kept(DispatchError::from_actor_error(
        ActorError::MailboxFull,
        3
    )));
    assert!(!kept(DispatchError::AuthzDenied(
        "no policy permits it".into()
    )));
    assert!(!kept(DispatchError::Conflict("a newer row".into())));
}
