use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::actor::context::ActorContext;
use crate::actor::traits::{Actor, Message};

/// What an init-failure observer recorded: the actor name and the kind it saw.
/// Named because the inline type trips `clippy::type_complexity`, which CI
/// promotes to an error with `-D warnings`.
type ObservedInitFailures = Arc<std::sync::Mutex<Vec<(String, Option<InitFailureKind>)>>>;

#[derive(Debug)]
struct Ping;
impl Message for Ping {}

/// An actor whose `pre_start` always fails with the supplied error.
struct NeverStarts {
    cause: Box<dyn Fn() -> ActorError + Send + Sync>,
    strategy: SupervisionStrategy,
    attempts: Arc<AtomicUsize>,
}

impl Actor for NeverStarts {
    type Msg = Ping;
    type State = ();

    fn supervision_strategy(&self) -> SupervisionStrategy {
        self.strategy.clone()
    }

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err((self.cause)())
    }

    async fn handle(
        &self,
        _msg: Ping,
        _state: &mut (),
        _ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        Ok(())
    }

    async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
}

fn never_starts(
    cause: impl Fn() -> ActorError + Send + Sync + 'static,
) -> (NeverStarts, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    (
        NeverStarts {
            cause: Box::new(cause),
            strategy: SupervisionStrategy::Stop,
            attempts: attempts.clone(),
        },
        attempts,
    )
}

#[tokio::test]
async fn pending_ask_gets_the_init_cause_not_stopped() {
    let (actor, _) = never_starts(|| {
        ActorError::init_failed(
            "duplicate declared key 'ws_path' for File: held by fl-019efda8",
            InitFailureKind::Constraint,
        )
    });
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-1")).spawn();

    let err = actor_ref
        .ask::<()>(Ping, Duration::from_secs(5))
        .await
        .expect_err("pre_start failed, so the ask cannot succeed");

    match err {
        ActorError::InitFailed { cause, kind } => {
            assert_eq!(kind, InitFailureKind::Constraint);
            assert!(
                cause.contains("ws_path") && cause.contains("fl-019efda8"),
                "the underlying cause must reach the caller, got: {cause}"
            );
        }
        other => panic!("expected InitFailed carrying the cause, got {other:?}"),
    }
}

#[tokio::test]
async fn every_queued_ask_is_answered() {
    // Several callers pile onto one cold entity; all of them must be told
    // why, not just the one at the head of the mailbox. `join!` polls every
    // ask once before any of them can await, so all four are enqueued
    // before the cell runs — no timing race.
    let (actor, _) = never_starts(|| {
        ActorError::init_failed("store unavailable", InitFailureKind::TransientDependency)
    });
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-2")).spawn();

    let budget = Duration::from_secs(5);
    let (a, b, c, d) = tokio::join!(
        actor_ref.ask::<()>(Ping, budget),
        actor_ref.ask::<()>(Ping, budget),
        actor_ref.ask::<()>(Ping, budget),
        actor_ref.ask::<()>(Ping, budget),
    );

    for result in [a, b, c, d] {
        let err = result.expect_err("pre_start failed, so the ask cannot succeed");
        assert!(
            err.is_transient(),
            "a transient dependency failure must stay transient, got {err:?}"
        );
        assert_eq!(
            err.init_failure_kind(),
            Some(InitFailureKind::TransientDependency)
        );
    }
}

#[tokio::test]
async fn unclassified_pre_start_failure_is_reported_as_a_defect() {
    let (actor, attempts) = never_starts(|| ActorError::custom("table lock poisoned"));
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-3")).spawn();

    let err = actor_ref
        .ask::<()>(Ping, Duration::from_secs(5))
        .await
        .expect_err("pre_start failed, so the ask cannot succeed");

    assert_eq!(err.init_failure_kind(), Some(InitFailureKind::Defect));
    assert!(err.is_permanent());
    assert!(err.to_string().contains("table lock poisoned"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "Stop strategy: one try");
}

#[tokio::test]
async fn init_failure_is_reported_after_restarts_are_exhausted() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let actor = NeverStarts {
        cause: Box::new(|| {
            ActorError::init_failed("store unavailable", InitFailureKind::TransientDependency)
        }),
        strategy: SupervisionStrategy::Restart {
            max_retries: 2,
            backoff_base: Duration::from_millis(1),
        },
        attempts: attempts.clone(),
    };
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-4")).spawn();

    let err = actor_ref
        .ask::<()>(Ping, Duration::from_secs(5))
        .await
        .expect_err("pre_start failed on every attempt");

    assert_eq!(
        err.init_failure_kind(),
        Some(InitFailureKind::TransientDependency)
    );
    // 1 initial + 2 restarts, all inside the caller's ask.
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn mailbox_is_closed_so_later_senders_fail_fast() {
    let (actor, _) = never_starts(|| ActorError::init_failed("nope", InitFailureKind::Defect));
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-5")).spawn();

    let _ = actor_ref.ask::<()>(Ping, Duration::from_secs(5)).await;

    // The cell is gone; a send must be refused immediately rather than
    // queueing into a mailbox nobody reads.
    assert_eq!(actor_ref.tell(Ping), Err(ActorError::SendFailed));
}

/// The regression that matters most: an init failure nobody is waiting for.
/// The spawner recorded this actor before it started, so the retraction
/// cannot be conditional on a caller turning up to receive the error.
#[tokio::test]
async fn observer_fires_when_no_caller_ever_asks() {
    let seen: ObservedInitFailures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let (actor, _) = never_starts(|| {
        ActorError::init_failed("store unavailable", InitFailureKind::TransientDependency)
    });
    let _actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-6"))
        .with_init_failure_observer(Arc::new(move |id: &ActorId, err: &ActorError| {
            recorder
                .lock()
                .unwrap()
                .push((id.path.clone(), err.init_failure_kind()));
        }))
        .spawn();

    // No ask, no tell — just wait for the cell to give up on its own.
    for _ in 0..500 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        1,
        "an unwatched init failure must still be reported exactly once, got {seen:?}"
    );
    assert_eq!(seen[0].0, "test/file-6");
    assert_eq!(
        seen[0].1,
        Some(InitFailureKind::TransientDependency),
        "the observer needs the classification to decide what to retract"
    );
}

#[tokio::test]
async fn observer_runs_before_any_caller_is_answered() {
    let retracted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = retracted.clone();
    let (actor, _) = never_starts(|| {
        ActorError::init_failed("duplicate declared key 'path'", InitFailureKind::Constraint)
    });
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-7"))
        .with_init_failure_observer(Arc::new(move |_: &ActorId, _: &ActorError| {
            flag.store(true, Ordering::SeqCst);
        }))
        .spawn();

    let err = actor_ref
        .ask::<()>(Ping, Duration::from_secs(5))
        .await
        .expect_err("pre_start failed, so the ask cannot succeed");

    assert_eq!(err.init_failure_kind(), Some(InitFailureKind::Constraint));
    assert!(
        retracted.load(Ordering::SeqCst),
        "a caller handed an init failure must find the spawn already retracted, \
         or it races the cleanup it depends on"
    );
}

#[tokio::test]
async fn observer_fires_once_after_restarts_are_exhausted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let actor = NeverStarts {
        cause: Box::new(|| {
            ActorError::init_failed("store unavailable", InitFailureKind::TransientDependency)
        }),
        strategy: SupervisionStrategy::Restart {
            max_retries: 2,
            backoff_base: Duration::from_millis(1),
        },
        attempts: attempts.clone(),
    };
    let actor_ref = ActorCell::new(actor, ActorId::new("file", "test/file-8"))
        .with_init_failure_observer(Arc::new(move |_: &ActorId, _: &ActorError| {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .spawn();

    let _ = actor_ref.ask::<()>(Ping, Duration::from_secs(5)).await;

    assert_eq!(attempts.load(Ordering::SeqCst), 3, "1 initial + 2 restarts");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a cell that is still retrying has not given up — retracting mid-retry \
         would un-register an actor that may yet start"
    );
}

#[tokio::test]
async fn observer_is_silent_when_init_succeeds() {
    struct Healthy;
    impl Actor for Healthy {
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
            ctx.reply(());
            Ok(())
        }
        async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let actor_ref = ActorCell::new(Healthy, ActorId::new("file", "test/file-9"))
        .with_init_failure_observer(Arc::new(move |_: &ActorId, _: &ActorError| {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .spawn();

    actor_ref
        .ask::<()>(Ping, Duration::from_secs(5))
        .await
        .expect("a healthy actor answers");
    actor_ref.stop().expect("stop is accepted");
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an actor that started and then stopped did not fail to initialize"
    );
}

#[test]
fn stop_strategy_never_restarts() {
    let strategy = SupervisionStrategy::Stop;
    assert!(!should_restart(&strategy, 0));
    assert!(!should_restart(&strategy, 1));
    assert!(!should_restart(&strategy, 100));
}

#[test]
fn restart_strategy_respects_max_retries() {
    let strategy = SupervisionStrategy::Restart {
        max_retries: 3,
        backoff_base: Duration::from_millis(100),
    };
    assert!(should_restart(&strategy, 0));
    assert!(should_restart(&strategy, 1));
    assert!(should_restart(&strategy, 2));
    assert!(!should_restart(&strategy, 3));
    assert!(!should_restart(&strategy, 4));
}

#[test]
fn restart_strategy_zero_retries_never_restarts() {
    let strategy = SupervisionStrategy::Restart {
        max_retries: 0,
        backoff_base: Duration::from_millis(100),
    };
    assert!(!should_restart(&strategy, 0));
}
