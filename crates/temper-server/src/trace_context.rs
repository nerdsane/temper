//! One piece of work is one span in one trace.
//!
//! A request's work stays in the request's trace, including the bookkeeping it
//! queues for later: the queue keeps the trace context of the span that queued
//! an item ([`capture`]), and the worker's span for that item continues it
//! ([`continue_in`]). Work that already runs inside a span never re-parents
//! itself onto a context it was handed: a span is only ever in the trace it was
//! created in, so nothing appears in two traces and no trace has two roots.
//!
//! Work the runtime starts on its own, after the move that caused it (a state
//! timer, a scheduled action, a spawned record, a background reaction, an
//! integration and the callback it dispatches, a decision's resolution that an
//! approve or deny request hands off, the governance callback), is a trace of
//! its own: its first span has no parent and links to the move that caused it
//! ([`runtime_work_span!`]). The trace ends, and its root is exported, when that
//! piece of work ends, so a record that keeps moving makes many short traces,
//! each linked to the move before it, instead of one trace that grows with
//! every later move.

use opentelemetry::trace::{SpanContext, TraceContextExt};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Open the root span of work the runtime starts on its own:
/// `runtime_work_span!(cause, kind = "state_timeout", "span.name", fields...)`.
///
/// The span has no parent, so the work is its own trace, exported when the work
/// ends. It links to `cause`, a [`SpanContext`] taken with [`cause`] where the
/// work was handed off: the span of the move that started it. Create the span
/// when the work starts (after a timer's wait, not before), so its duration is
/// the work's.
///
/// Every such root carries the attribute `runtime_work = kind`, the work's own
/// name. It is what marks a trace as runtime work (a trace pipeline keeps
/// `parent_id:0 @runtime_work:*`), so a new kind needs no rule anywhere else.
///
/// Exported (`temper_server::runtime_work_span!`) so work handed off outside
/// this crate, such as the platform's governance callback, follows the same
/// rule.
#[macro_export]
macro_rules! runtime_work_span {
    ($cause:expr, kind = $kind:literal, $name:literal $(, $($field:tt)*)?) => {{
        let span = ::tracing::info_span!(
            parent: None,
            $name,
            runtime_work = $kind
            $(, $($field)*)?
        );
        $crate::trace_context::link_cause(&span, &$cause);
        span
    }};
}
pub use runtime_work_span;

/// The span of the move that is handing off work, for the work's own trace to
/// link back to. Invalid when no span is active (a timer armed at boot).
pub fn cause() -> SpanContext {
    span_context(&tracing::Span::current())
}

/// Link the root span of runtime-started work to the move that caused it.
/// [`runtime_work_span!`] calls this; other code opens the span with the macro.
pub fn link_cause(span: &tracing::Span, cause: &SpanContext) {
    if cause.is_valid() {
        span.add_link(cause.clone());
    }
}

/// The current span's trace context, to be continued by work that runs later on
/// another task.
pub(crate) fn capture() -> opentelemetry::Context {
    tracing::Span::current().context()
}

/// Continue `parent` (captured with [`capture`] where the work was queued) in
/// `span`, a span the worker created without a parent and has not entered yet.
pub(crate) fn continue_in(span: &tracing::Span, parent: &opentelemetry::Context) {
    if parent.span().span_context().is_valid() {
        span.set_parent(parent.clone());
    }
}

/// The W3C `traceparent` header naming `span`, for an outgoing request made
/// inside it.
pub(crate) fn traceparent(span: &tracing::Span) -> Option<String> {
    let context = span_context(span);
    context.is_valid().then(|| {
        format!(
            "00-{}-{}-{:02x}",
            context.trace_id(),
            context.span_id(),
            context.trace_flags().to_u8()
        )
    })
}

pub(crate) fn span_context(span: &tracing::Span) -> SpanContext {
    span.context().span().span_context().clone()
}

#[cfg(test)]
#[path = "trace_context_test.rs"]
mod tests;
