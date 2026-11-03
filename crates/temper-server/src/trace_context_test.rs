use opentelemetry::trace::SpanId;

use super::*;
use crate::test_spans;

#[tokio::test]
async fn queued_work_continues_the_trace_that_queued_it() {
    let spans = test_spans::capture();
    let request = tracing::info_span!("request");
    let parent = request.in_scope(capture);
    let request_context = span_context(&request);
    drop(request);

    tokio::spawn(async move {
        let worker = tracing::info_span!(parent: None, "worker.item");
        continue_in(&worker, &parent);
        worker.in_scope(|| tracing::info_span!("worker.write").in_scope(|| {}));
    })
    .await
    .unwrap();

    let finished = spans.finished();
    let item = finished.iter().find(|s| s.name == "worker.item").unwrap();
    let write = finished.iter().find(|s| s.name == "worker.write").unwrap();
    assert_eq!(item.span_context.trace_id(), request_context.trace_id());
    assert_eq!(item.parent_span_id, request_context.span_id());
    assert_eq!(write.span_context.trace_id(), request_context.trace_id());
    assert_eq!(write.parent_span_id, item.span_context.span_id());
}

#[tokio::test]
async fn a_worker_span_with_nothing_to_continue_is_its_own_root() {
    let spans = test_spans::capture();
    let worker = tracing::info_span!(parent: None, "worker.item");
    continue_in(&worker, &opentelemetry::Context::new());
    drop(worker);
    let finished = spans.finished();
    assert_eq!(finished[0].parent_span_id, SpanId::INVALID);
}

#[test]
fn runtime_work_is_its_own_trace_linked_to_the_move_that_started_it() {
    let spans = test_spans::capture();
    let request = tracing::info_span!(parent: None, "request");
    let (started_by, work) = request.in_scope(|| {
        let started_by = cause();
        let work = runtime_work_span!(started_by.clone(), "work.item", work.kind = "timer");
        (started_by, work)
    });
    drop(request);
    work.in_scope(|| tracing::info_span!("work.step").in_scope(|| {}));
    drop(work);

    let finished = spans.finished();
    let item = finished.iter().find(|s| s.name == "work.item").unwrap();
    let step = finished.iter().find(|s| s.name == "work.step").unwrap();
    assert_eq!(item.parent_span_id, SpanId::INVALID, "the work is a root");
    assert_ne!(item.span_context.trace_id(), started_by.trace_id());
    let links: Vec<_> = item.links.iter().map(|l| l.span_context.clone()).collect();
    assert_eq!(
        links,
        vec![started_by],
        "linked back to the move that started it"
    );
    assert_eq!(step.span_context.trace_id(), item.span_context.trace_id());
    assert_eq!(step.parent_span_id, item.span_context.span_id());
}

#[test]
fn runtime_work_with_no_cause_is_an_unlinked_root() {
    let spans = test_spans::capture();
    let work = runtime_work_span!(cause(), "work.item");
    drop(work);
    let finished = spans.finished();
    assert_eq!(finished[0].parent_span_id, SpanId::INVALID);
    assert!(finished[0].links.is_empty());
}

#[test]
fn traceparent_names_the_span() {
    let _spans = test_spans::capture();
    let span = tracing::info_span!("client");
    let context = span_context(&span);
    assert_eq!(
        traceparent(&span).unwrap(),
        format!("00-{}-{}-01", context.trace_id(), context.span_id())
    );
}
