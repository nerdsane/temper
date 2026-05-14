# ADR-0087: WASM Guest Observability Host API

- Status: Accepted
- Date: 2026-05-14

## Context

ADR-0086 made the WASM host boundary visible in Datadog with host-call spans,
guest progress correlation, structured guest logs, metrics, and span-hint
headers for outbound HTTP. That is enough to see when a guest crosses a host
function, but it is not enough for pure in-guest work: a module cannot create
its own nested spans, add span events, or mark a guest span as failed without
going through an unrelated host boundary.

TemperPaw now needs provider/tool execution spans that continue the same trace
inside WASM and correlate logs, metrics, host calls, and Datadog APM views.

## Decision

Temper exposes a structured guest observability ABI:

- `host_start_span(payload_ptr, payload_len) -> i64`
- `host_add_span_event(span_id, payload_ptr, payload_len) -> i32`
- `host_set_span_attributes(span_id, payload_ptr, payload_len) -> i32`
- `host_end_span(span_id, payload_ptr, payload_len) -> i32`

Payloads are JSON. `host_start_span` requires `name` and accepts optional
`kind` and `attributes`. Span ids are opaque positive integers scoped to one
WASM invocation. Guest spans nest by start/end order; the active guest span is
entered around existing telemetry host functions so logs, metrics, progress,
wide events, and host-boundary spans remain correlated.

Guest metrics remain low-cardinality OTEL metrics. The host also emits a
`wasm_guest.metric` event on the active guest span so operators can pivot from
the trace to the metric without adding trace ids as metric tags.

Temper closes the OpenTelemetry span attached to the tracing span when the
guest ends or when invocation cleanup closes an unended span. That keeps the
same span id usable for APM and log correlation even for WASI guests that do
most work between host function calls.

## Consequences

- WASM modules can create Datadog-visible child spans for useful pure guest
  work instead of abusing outbound HTTP span hints.
- Existing `X-Temper-Span-*` hints remain supported for old modules.
- The host owns span ids, limits, cleanup of unclosed spans, reserved
  observability fields, and attribute truncation.
- Guests do not receive raw OpenTelemetry objects or trace ids as control
  handles.
