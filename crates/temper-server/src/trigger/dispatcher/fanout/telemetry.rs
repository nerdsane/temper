//! Low-cardinality reaction fanout telemetry.

#[derive(Default)]
pub(super) struct ReactionFanoutCounts {
    pub(super) rule_count: usize,
    pub(super) fired_count: usize,
    pub(super) guard_skipped_count: usize,
    pub(super) target_resolve_error_count: usize,
    pub(super) authz_denied_count: usize,
    pub(super) dispatch_error_count: usize,
    pub(super) success_count: usize,
    pub(super) result_count: usize,
}

pub(super) fn record_reaction_fanout_span(counts: ReactionFanoutCounts) {
    let span = tracing::Span::current();
    span.record("reaction.rule_count", counts.rule_count);
    span.record("reaction.fired_count", counts.fired_count);
    span.record("reaction.guard_skipped_count", counts.guard_skipped_count);
    span.record(
        "reaction.target_resolve_error_count",
        counts.target_resolve_error_count,
    );
    span.record("reaction.authz_denied_count", counts.authz_denied_count);
    span.record("reaction.dispatch_error_count", counts.dispatch_error_count);
    span.record("reaction.success_count", counts.success_count);
    span.record("reaction.result_count", counts.result_count);
}
