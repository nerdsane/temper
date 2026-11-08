//! Sending a batch again when the collector resets the connection.

use crate::*;

/// Around a handoff the collector's end resets the first connections a new
/// process opens: on 30 Oct 2026 every staging handoff lost the spans of a
/// boot's first second this way, among them opening the store. A failed
/// batch is sent again, so the spans and logs still arrive, each once.
#[test]
fn a_batch_sent_while_the_collector_resets_connections_arrives_once() {
    let run = child::run_with_resets(2, &[]);
    assert_eq!(span_names(&run), SPANS_FOLLOWING_THE_CALLER);
    let exported: Vec<String> = run.logs().into_iter().map(|record| record.body).collect();
    for line in ["test log inside a span", "test log outside a span"] {
        let copies = exported.iter().filter(|body| *body == line).count();
        assert_eq!(copies, 1, "{line:?} in {exported:?}");
    }
}
