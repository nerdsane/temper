use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::*;

async fn execute(sandbox: &mut PersistentSandbox, code: &str) -> Result<String> {
    sandbox
        .execute(code, |_, _, _| async { Ok(Value::Null) })
        .await
}

#[tokio::test]
async fn independent_turns_can_exceed_the_cumulative_allocation_limit() {
    let mut sandbox = PersistentSandbox::new(&[]);
    execute(&mut sandbox, "global saved\nsaved = 42")
        .await
        .unwrap();
    // At least 360,000 string allocations, while each individual turn is small.
    // The former snapshot/load tracker exhausted its lifetime 250,000 budget.
    for _ in 0..120 {
        assert_eq!(
            execute(
                &mut sandbox,
                "values = [str(i) for i in range(3000)]\nreturn saved"
            )
            .await
            .unwrap(),
            "42"
        );
    }
}

#[tokio::test]
async fn a_single_over_budget_turn_fails_and_the_next_turn_recovers() {
    let mut sandbox = PersistentSandbox::new(&[]);
    execute(&mut sandbox, "global saved\nsaved = 42")
        .await
        .unwrap();
    let error = execute(
        &mut sandbox,
        "for i in range(250001):\n    value = str(i)\nreturn True",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("allocation"), "{error}");
    assert_eq!(execute(&mut sandbox, "return saved").await.unwrap(), "42");
}

#[tokio::test]
async fn external_call_resume_cannot_restart_the_turn_budget() {
    let mut sandbox = PersistentSandbox::new(&[("temper", "Temper", 1)]);
    sandbox.tracker = TurnTracker::new(default_limits().max_allocations(256));
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatch_calls = calls.clone();
    let error = sandbox.execute(
        "before = [str(i) for i in range(160)]\nawait temper.echo()\nafter = [str(i) for i in range(160)]\nreturn True",
        move |_, _, _| {
            dispatch_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(Value::Null) }
        },
    ).await.unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the failure must occur after suspension"
    );
    assert!(error.to_string().contains("allocation"), "{error}");
    assert_eq!(
        execute(&mut sandbox, "return {'connected': True}")
            .await
            .unwrap(),
        r#"{"connected":true}"#
    );
}
