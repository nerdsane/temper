# ADR-0083: WASM Host Span Hints Must Be Datadog-Visible

Date: 2026-05-12

## Status

Accepted

## Context

Temper WASM guests can annotate outbound host HTTP calls with
`X-Temper-Span-Name` and `X-Temper-Span-Attr-*` headers. The host consumes those
headers before forwarding the request and applies them to the local tracing span.

In live Datadog verification for TemperPaw, generic WASM host HTTP spans were
visible, but hinted semantic names and important correlation attributes were not
reliably searchable as Datadog operation/resource names or facets. That made the
trace usable for low-level HTTP timing, but not good enough for human or agent
debugging of a full agent session.

## Decision

The WASM host now treats span hints as first-class Datadog-facing fields:

1. `X-Temper-Span-Name` is used as the span's initial `otel.name` field when the
   host span is created, in addition to updating the underlying OpenTelemetry
   span name.
2. Common session, entity, workflow, tool, and LLM hint attributes are declared
   as static tracing fields on host HTTP spans and recorded from the hint set.
3. The host still writes all hint attributes to the underlying OpenTelemetry span
   so future/custom attributes are not discarded.
4. Hint headers continue to be stripped before outbound requests leave the host.
5. Guest log span events include the active OpenTelemetry trace/span ids and
   Datadog decimal `dd.trace_id` / `dd.span_id` fields. If a WASM invocation is
   acting on a `Session` entity and no explicit `session_id` exists, the host
   derives `session_id` from the entity id so logs remain joinable to the
   session trace.

The Datadog-visible static fields currently include:

- `observability_event`
- `session_id`
- `managed_session_id`
- `inner_session_id`
- `parent_session_id`
- `agent_id`
- `environment_id`
- `entity_type`
- `entity_id`
- `action_name`
- `workflow_step`
- `tool.name`
- `tool.call_id`
- `gen_ai.operation.name`
- `gen_ai.provider.name`
- `gen_ai.request.model`

## Consequences

Human operators and agents can search host HTTP spans by the same session/entity
vocabulary used in TemperPaw logs, OData state, LLM Observability, and monitors.
This makes WASM host calls less opaque without introducing a guest-managed span
ABI yet.

Guest logs emitted through the host are also joinable to the same trace by both
OpenTelemetry hex ids and Datadog decimal ids. This is intentionally host-side:
guests do not need direct access to tracing internals to produce correlated logs.

This ADR does not claim true inside-WASM APM. Guest code still cannot create
arbitrary child spans directly. If future debugging requires that, Temper should
add an explicit guest-to-host observability API such as `host_start_span`,
`host_add_span_event`, and `host_end_span`.

The field allowlist must remain intentionally small. High-cardinality or
payload-sized values should stay in logs, span events, OData rows, or LLMObs
content fields instead of becoming universal trace facets.
