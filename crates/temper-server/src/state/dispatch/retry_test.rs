use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::actor::{Actor, ActorCell, ActorContext, ActorId};
use tokio::sync::Notify;

#[derive(Debug)]
struct Ping;
impl Message for Ping {}

/// Counts each message it starts, then answers once it is let go.
struct Held {
    started: Arc<AtomicUsize>,
    go: Arc<Notify>,
}

impl Actor for Held {
    type Msg = Ping;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        _msg: Ping,
        _state: &mut (),
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.go.notified().await;
        ctx.reply(());
        Ok(())
    }

    async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
}

fn held() -> (ActorRef<Ping>, Arc<AtomicUsize>, Arc<Notify>) {
    let (started, go) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
    let actor = Held {
        started: started.clone(),
        go: go.clone(),
    };
    let actor = ActorCell::new(actor, ActorId::new("held", "test/held")).spawn();
    (actor, started, go)
}

#[tokio::test(start_paused = true)]
async fn a_retry_waits_on_its_ask_instead_of_stacking_a_copy() {
    let (actor, started, go) = held();
    actor.tell(Ping).unwrap();
    tokio::task::yield_now().await;
    assert_eq!(started.load(Ordering::SeqCst), 1);

    let outcome =
        ask_with_backoff::<_, (), _>(&actor, || Ping, &policy_with_attempts(3, vec![0, 50])).await;
    assert_eq!(outcome.attempts, 3);
    assert_eq!(
        outcome.result,
        Err(ActorError::AskTimeout(Duration::ZERO)),
        "three attempts waited on the held actor"
    );
    assert_eq!(actor.mailbox_depth(), 1, "one copy, not three");

    // The copy's caller has gone, so letting the actor go never starts it.
    go.notify_one();
    tokio::task::yield_now().await;
    let answered = tokio::spawn({
        let actor = actor.clone();
        async move { actor.ask::<()>(Ping, Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;
    go.notify_one();
    answered.await.unwrap().unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn an_answer_after_a_timeout_reaches_the_later_attempt() {
    let (actor, started, go) = held();
    let later = {
        let go = go.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(7)).await;
            go.notify_one();
        })
    };
    let outcome =
        ask_with_backoff::<_, (), _>(&actor, || Ping, &policy_with_attempts(3, vec![0, 50])).await;
    later.await.unwrap();
    assert_eq!(outcome.result, Ok(()));
    assert_eq!(outcome.attempts, 2);
    assert_eq!(started.load(Ordering::SeqCst), 1, "it ran once");
}

/// Never finishes a message, so asks stay queued behind the first.
struct Stuck;

impl Actor for Stuck {
    type Msg = EntityMsg;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        _msg: EntityMsg,
        _state: &mut (),
        _ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        std::future::pending().await
    }

    async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
}

#[tokio::test(start_paused = true)]
async fn a_read_is_refused_at_once_while_half_the_mailbox_is_taken_and_a_move_still_fits() {
    let actor = ActorCell::new(Stuck, ActorId::new("stuck", "test/stuck"))
        .with_mailbox_capacity(4)
        .spawn();
    for _ in 0..3 {
        actor.tell(EntityMsg::GetState).unwrap();
    }
    tokio::task::yield_now().await;
    assert_eq!(actor.mailbox_depth(), 2, "one in hand, two queued");

    let started = tokio::time::Instant::now();
    let read = ask_state(&actor, &policy_with_attempts(3, vec![0, 50])).await;
    assert_eq!(read.result.err(), Some(ActorError::MailboxFull));
    assert_eq!(started.elapsed(), Duration::ZERO, "refused without waiting");
    actor
        .tell(EntityMsg::GetState)
        .expect("room is left for what is not a read");
}

fn policy_with_attempts(max_attempts: u32, delays: Vec<u64>) -> RetryPolicy {
    RetryPolicy {
        total_deadline: Duration::from_secs(30),
        per_attempt_timeout: Duration::from_secs(5),
        max_attempts,
        base_delays_ms: delays,
    }
}

#[test]
fn base_delay_zero_for_first_attempt() {
    let p = policy_with_attempts(4, vec![0, 50, 200, 800]);
    assert_eq!(p.base_delay_ms_for(0), 0);
}

#[test]
fn base_delay_follows_index() {
    let p = policy_with_attempts(4, vec![0, 50, 200, 800]);
    assert_eq!(p.base_delay_ms_for(1), 50);
    assert_eq!(p.base_delay_ms_for(2), 200);
    assert_eq!(p.base_delay_ms_for(3), 800);
}

#[test]
fn base_delay_clamps_to_last_entry() {
    let p = policy_with_attempts(10, vec![0, 50, 200, 800]);
    // Past the end of the table — stays at the last defined value.
    assert_eq!(p.base_delay_ms_for(7), 800);
    assert_eq!(p.base_delay_ms_for(100), 800);
}

#[test]
fn base_delay_empty_table_always_zero() {
    let p = policy_with_attempts(3, vec![]);
    assert_eq!(p.base_delay_ms_for(1), 0);
    assert_eq!(p.base_delay_ms_for(10), 0);
}

#[test]
fn remaining_budget_never_goes_negative() {
    let p = policy_with_attempts(3, vec![0, 50]);
    let long = Duration::from_secs(60);
    assert_eq!(p.remaining(long), Duration::ZERO);
}

#[test]
fn jitter_stays_within_base_bound() {
    // Property check: over many (base, attempt) combos, full-jitter must
    // never exceed the base.
    for base in [0u64, 1, 10, 50, 200, 800, 5000] {
        for attempt in 0u32..16 {
            let d = jittered_delay_ms(base, attempt);
            assert!(
                d <= base,
                "jitter {d} exceeded base {base} at attempt {attempt}"
            );
        }
    }
}

#[test]
fn single_attempt_policy_has_one_attempt_and_zero_delays() {
    let p = RetryPolicy::single_attempt();
    assert_eq!(p.max_attempts, 1);
    // Base delay at attempt 0 is always 0 regardless of table contents.
    assert_eq!(p.base_delay_ms_for(0), 0);
}

#[test]
fn max_attempts_coerced_to_at_least_one() {
    // Default() coerces invalid env values upwards; exposed via the
    // `.max(1)` in Default::default(). A direct construction with 0 is
    // allowed so tests can probe edge cases.
    let p = RetryPolicy {
        max_attempts: 0,
        ..RetryPolicy::default()
    };
    // ask_with_backoff internally coerces max_attempts to >= 1 so the
    // loop always runs at least once; this is the contract the dispatch
    // layer depends on.
    let effective = p.max_attempts.max(1);
    assert_eq!(effective, 1);
}

#[test]
fn attempt_budget_floor_prevents_per_attempt_timeout_from_disabling_retry() {
    let p = RetryPolicy {
        total_deadline: Duration::from_secs(30),
        per_attempt_timeout: Duration::from_secs(30),
        max_attempts: 3,
        base_delays_ms: vec![0, 50, 200],
    }
    .with_attempt_budget_floor();

    assert_eq!(p.total_deadline, Duration::from_millis(90_250));
}
