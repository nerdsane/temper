use super::*;
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

// Poll each caller into the queue before submitting the holder's work. This
// exercises the actual lock path without relying on a scheduler sleep.
async fn poll_waiting(future: std::pin::Pin<&mut impl Future>) {
    let mut future = future;
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

fn holder_can_use_blocking_pool(threads: usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(threads)
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("turn.lock");
        let holder = take_at(path.clone()).await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..threads * 2 {
            let mut waiter = Box::pin(take_at(path.clone()));
            poll_waiting(waiter.as_mut()).await;
            waiters.push(waiter);
        }

        // Verification and joint proofs need the same pool while holding a
        // turn. Queued callers must leave it free for that work.
        let mut work = tokio::task::spawn_blocking(|| 42);
        let result = tokio::time::timeout(Duration::from_secs(2), &mut work).await;
        let starved = result.is_err();
        if let Ok(result) = result {
            assert_eq!(result.unwrap(), 42);
        }
        // Release everything even on the defective implementation, so a
        // failing test reports starvation instead of hanging at shutdown.
        drop(waiters);
        drop(holder);
        if starved {
            work.await.unwrap();
        }
        assert!(
            !starved,
            "waiting callers exhausted the {threads}-thread pool"
        );
    });
}

#[test]
fn holder_progresses_with_two_blocking_threads() {
    holder_can_use_blocking_pool(2);
}

#[test]
fn holder_progresses_with_eight_blocking_threads() {
    holder_can_use_blocking_pool(8);
}

#[tokio::test]
async fn turn_keeps_the_file_lock_until_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("turn.lock");
    let holder = take_at(path.clone()).await.unwrap();
    let other = OpenOptions::new().write(true).open(&path).unwrap();
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let mut waiter = Box::pin(take_at(path));
    poll_waiting(waiter.as_mut()).await;
    drop(holder);
    let next = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(next);
    other.try_lock().unwrap();
}

#[tokio::test]
async fn open_failure_releases_the_turn() {
    let directory = tempfile::tempdir().unwrap();
    let result = take_at(directory.path().join("missing/turn.lock")).await;
    assert!(result.err().unwrap().contains("could not be opened"));
    let holder = tokio::time::timeout(
        Duration::from_secs(2),
        take_at(directory.path().join("turn.lock")),
    )
    .await
    .unwrap()
    .unwrap();
    drop(holder);
}

#[test]
fn cancelled_lock_wait_keeps_other_callers_out_of_the_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("turn.lock");
        // A separate handle stands in for a sibling process holding the
        // host lock. This process must still wait for the OS lock.
        let external = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        external.lock().unwrap();
        let mut cancelled = Box::pin(take_at(path.clone()));
        poll_waiting(cancelled.as_mut()).await;
        drop(cancelled);

        let mut waiters = Vec::new();
        for _ in 0..4 {
            let mut waiter = Box::pin(take_at(path.clone()));
            poll_waiting(waiter.as_mut()).await;
            waiters.push(waiter);
        }
        let mut work = tokio::task::spawn_blocking(|| ());
        let starved = tokio::time::timeout(Duration::from_secs(2), &mut work)
            .await
            .is_err();
        drop(waiters);
        drop(external);
        if starved {
            work.await.unwrap();
        }
        assert!(
            !starved,
            "cancelling a lock wait let callers exhaust the pool"
        );
        // The detached acquisition must eventually drop both locks.
        let next = tokio::time::timeout(Duration::from_secs(2), take_at(path))
            .await
            .unwrap()
            .unwrap();
        drop(next);
    });
}
