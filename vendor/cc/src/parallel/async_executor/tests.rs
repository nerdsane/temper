use super::{backoff_duration, block_on};
use crate::{Error, ErrorKind};
use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

#[test]
fn backoff_keeps_initial_yields_and_exact_host_intervals() {
    let quantum_ms = if cfg!(target_os = "linux") { 1 } else { 100 };
    for count in 0..=3 {
        assert_eq!(backoff_duration(count), Duration::ZERO);
    }
    for (count, steps) in [(4, 1), (5, 2), (12, 9), (13, 10)] {
        assert_eq!(
            backoff_duration(count),
            Duration::from_millis(quantum_ms * steps)
        );
    }
}

#[test]
fn idle_backoff_is_bounded_without_busy_polling() {
    let cap_ms = if cfg!(target_os = "linux") { 10 } else { 1_000 };
    for count in [13, 14, 1_000_000, u64::MAX] {
        assert_eq!(backoff_duration(count), Duration::from_millis(cap_ms));
    }
    assert!(backoff_duration(4) > Duration::ZERO);
}

struct TrackedFuture<'a> {
    pending_polls: usize,
    fails: bool,
    polls: &'a Cell<usize>,
    completed: &'a Cell<bool>,
    dropped: &'a Cell<bool>,
}

impl Future for TrackedFuture<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(!this.completed.get(), "completed future polled again");
        this.polls.set(this.polls.get() + 1);
        if this.pending_polls > 0 {
            this.pending_polls -= 1;
            return Poll::Pending;
        }
        this.completed.set(true);
        Poll::Ready(if this.fails {
            Err(Error::new(ErrorKind::ToolExecError, "regression-test failure"))
        } else {
            Ok(())
        })
    }
}

impl Drop for TrackedFuture<'_> {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[test]
fn both_futures_complete_without_repolling_completed_work() {
    let progress = Cell::new(false);
    let first_polls = Cell::new(0);
    let second_polls = Cell::new(0);
    let first_completed = Cell::new(false);
    let second_completed = Cell::new(false);
    let first_dropped = Cell::new(false);
    let second_dropped = Cell::new(false);
    let first = TrackedFuture {
        pending_polls: 6,
        fails: false,
        polls: &first_polls,
        completed: &first_completed,
        dropped: &first_dropped,
    };
    let second = TrackedFuture {
        pending_polls: 1,
        fails: false,
        polls: &second_polls,
        completed: &second_completed,
        dropped: &second_dropped,
    };

    assert!(block_on(first, second, &progress).is_ok());
    assert_eq!(first_polls.get(), 7);
    assert_eq!(second_polls.get(), 2);
    assert!(first_completed.get() && second_completed.get());
    assert!(first_dropped.get() && second_dropped.get());
}

#[test]
fn error_from_either_future_cancels_and_drops_pending_work() {
    for first_fails in [false, true] {
        let progress = Cell::new(false);
        let first_polls = Cell::new(0);
        let second_polls = Cell::new(0);
        let first_completed = Cell::new(false);
        let second_completed = Cell::new(false);
        let first_dropped = Cell::new(false);
        let second_dropped = Cell::new(false);
        let first = TrackedFuture {
            pending_polls: if first_fails { 6 } else { 100 },
            fails: first_fails,
            polls: &first_polls,
            completed: &first_completed,
            dropped: &first_dropped,
        };
        let second = TrackedFuture {
            pending_polls: if first_fails { 100 } else { 6 },
            fails: !first_fails,
            polls: &second_polls,
            completed: &second_completed,
            dropped: &second_dropped,
        };

        let error = block_on(first, second, &progress).unwrap_err();
        assert_eq!(error.message, "regression-test failure");
        assert_eq!(first_completed.get(), first_fails);
        assert_eq!(second_completed.get(), !first_fails);
        assert!(first_dropped.get() && second_dropped.get());
        // The spawn future is polled first in the upstream executor.
        assert_eq!(second_polls.get(), 7);
        assert_eq!(first_polls.get(), if first_fails { 7 } else { 6 });
    }
}
