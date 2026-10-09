//! Real OTel roots must outlive request detachment, not their owned completion.
use super::tests::fixture::*;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use std::time::Duration;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;
use tracing_subscriber::prelude::*;

#[tokio::test(start_paused = true)]
async fn cancelled_request_does_not_lose_owned_terminal_workflow_drain() {
    let (_guard, _, _) = install_deterministic_context(51992);
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("owned-workflow-test")));
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let root = reaction_spec("Session", &["Child"]).replace("Running", "Completed");
    let child = reaction_spec("Child", &[]);
    let mut state = state_with_specs(&[("Session", &root), ("Child", &child)]);
    let store = SimEventStore::no_faults(51992);
    let barriers = gate_commits(&mut state, store.clone(), &["Child"]);
    // Parallel tests can register these callsites without this scoped tracer.
    // Refresh interest before establishing the real traced fixture.
    tracing::callsite::rebuild_interest_cache();
    // A real workflow root from an earlier request is the fixture precondition.
    // Keep its creation separate from concurrent dispatch-callsite registration.
    assert!(
        state
            .workflow_spans
            .parent_context("default", "Session", "one", "Session:one", None)
            .is_some()
    );
    let began = tokio::time::Instant::now();
    let mut caller = Box::pin(run(&state, "Session", false, true));
    tokio::select! { biased;
        result = &mut caller => panic!("root returned before reaction: {result:?}"),
        _ = barriers["Child"].entered.notified() => {}
    }
    assert!(
        state.workflow_spans.contains_run("Session:one"),
        "root absent at child gate after {:?}; exported {:?}",
        began.elapsed(),
        exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .map(|span| span.name.to_string())
            .collect::<Vec<_>>()
    );
    drop(caller);
    barriers["Child"].release.add_permits(1);
    // Observe completion without dispatching another request that could itself
    // close the root and conceal the cancelled-caller defect.
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(
        !state.workflow_spans.contains_run("Session:one"),
        "completed root retained after its requester detached"
    );
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "Session.workflow")
            .count(),
        1
    );
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "temper.workflow.drain_grace")
            .count(),
        1
    );
}
