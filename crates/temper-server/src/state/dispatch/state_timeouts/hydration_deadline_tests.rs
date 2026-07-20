//! Snapshot-anchor and absolute-deadline hydration regressions.

use super::*;

#[test]
fn snapshot_anchor_survives_an_empty_recent_event_window() {
    let reset_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    assert_eq!(
        compute_state_clock_reset_ts(&VecDeque::new(), Some(reset_at), "Running", &[]),
        Some(reset_at),
        "a current snapshot must retain the durable timeout anchor"
    );
}

#[tokio::test(start_paused = true)]
async fn absolute_deadline_survives_timer_task_poll_delay() {
    let deadline = timeout_deadline(Duration::from_secs(10));

    // Model a spawned timer task that receives no CPU for four seconds.
    tokio::time::advance(Duration::from_secs(4)).await;
    let timer = tokio::spawn(async move { tokio::time::sleep_until(deadline).await });
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_millis(5_999)).await;
    tokio::task::yield_now().await;
    assert!(
        !timer.is_finished(),
        "the timer must not fire before its deadline"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    timer
        .await
        .expect("task queue time must not move the precomputed deadline later");
}
