use super::*;

fn key() -> TimerRef {
    TimerRef::State {
        tenant: "alpha".into(),
        entity_type: "Job".into(),
        entity_id: "repeating".into(),
    }
}

/// What a report would name now.
fn earliest(due: &DueTimes) -> Option<DateTime<Utc>> {
    let inner = due.lock();
    let now = sim_now();
    inner.reportable(inner.horizon(now, None), now)
}

#[test]
fn a_newer_arm_keeps_its_due_time_when_the_older_task_ends() {
    let due = Arc::new(DueTimes::default());
    let now = sim_now();
    let first = due.arm(key(), now + chrono::Duration::minutes(5));
    // A reset arms the record's next timer before the old task ends.
    let next = now + chrono::Duration::hours(1);
    let successor = due.arm(key(), next);
    due.end(&first);
    assert_eq!(earliest(&due), Some(next));
    // The old task's retry does not move its successor either.
    due.retry_in(&first, Duration::from_secs(600));
    assert_eq!(earliest(&due), Some(next));
    let later = due.retry_in(&successor, Duration::from_secs(7200));
    assert!(later >= now + chrono::Duration::hours(2));
    assert_eq!(earliest(&due), Some(later));
    due.end(&successor);
    assert_eq!(earliest(&due), None);
}
