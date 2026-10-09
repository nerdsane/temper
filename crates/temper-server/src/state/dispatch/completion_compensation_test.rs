//! Direct defense-in-depth guard coverage, not a claimed public fallthrough path.
use super::{precheck_tests::receipt, tests::fixture::*};
use crate::request_context::LocalCompletionEvidence;
use std::time::Duration;
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

const SPEC: &str = r#"
[automaton]
name="Fallback"
states=["Idle","Running","Failed"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action]]
name="Fail"
from=["Running"]
to="Failed"
"#;

async fn compensate(unknown: bool) {
    let mut state = state_with_specs(&[("Fallback", SPEC)]);
    let store = SimEventStore::no_faults(52001);
    state.set_storage_stack(crate::StorageStack::from_sim(store.clone(), None));
    if unknown {
        *state
            .idempotency_cache
            .interrupt_action_once
            .lock()
            .unwrap() = Some(("default".into(), "Fallback".into(), "Start".into()));
    }
    let first = run(&state, "Fallback", true, true).await;
    assert_eq!(first.success, !unknown);
    let receipt = receipt(&state, "default", "Fallback", "one", "K");
    let mut context = receipt.request.agent_ctx.clone();
    context.local_completion = LocalCompletionEvidence::for_operation(&receipt);
    if unknown {
        context.local_completion.mark_unknown();
    }
    // Ordinary inline owned failures do not naturally invoke this background
    // helper. Exercise its guard directly with an actual retained receipt and
    // no declared result callback, plus the same quiescent fallback control.
    state.dispatch_integration_failure_compensation(
        &TenantId::default(),
        "Fallback",
        "one",
        "Start",
        "injected integration failure",
        &context,
    );
    if unknown {
        for _ in 0..128 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            action_count(&store, "Fallback", "Fail"),
            0,
            "tracked Unknown launched implicit compensation"
        );
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            63
        );
    } else {
        tokio::time::timeout(Duration::from_secs(1), async {
            while action_count(&store, "Fallback", "Fail") == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ordinary quiescent failure lost fallback");
        assert_eq!(action_count(&store, "Fallback", "Fail"), 1);
    }
}
#[tokio::test]
async fn tracked_unknown_without_declared_callback_cannot_compensate() {
    compensate(true).await;
}
#[tokio::test]
async fn quiescent_failure_without_declared_callback_still_compensates() {
    compensate(false).await;
}
