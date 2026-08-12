# ADR-0164: Runtime Action Observability Metadata

- Status: Accepted
- Date: 2026-06-08
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/request_context.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs`
  - `crates/temper-observe/src/wide_event.rs`

## Context

Temper apps already emit transition telemetry, but Datadog logs can still be
hard to read when the only visible message is a generic trajectory row. Humans
and observer agents need a stable platform-level view of app usage: which tenant,
entity, action, status transition, session, intent, and producer-supplied
correlation metadata caused the runtime action.

Different producers may need different correlation keys. Directed Evolution may
need work item and simulated-user identifiers, workflow orchestration may need
run identifiers, and support tooling may need ticket identifiers. Temper core
should not encode those producer-specific vocabularies as first-class fields.

## Decision

Temper runtime request context will extract generic observability metadata from
`X-Temper-Observe-Metadata` JSON and `X-Temper-Observe-Meta-*` headers. Producers
must namespace their own keys, such as `workflow.run_id` or
`producer.work_item_id`. Temper treats these keys as opaque metadata and does
not branch on their meaning.

The dispatch path will emit readable application usage logs for entity actions
and include generic session, intent, and serialized observation metadata when
present. OData request spans will project each metadata key as a dynamic
`temper.observation.<key>` span attribute.

Transition wide-event spans will include tenant context so Datadog trace
queries scoped by runtime tenant can find app-level transitions such as
`Question.Configure` and `Answer.Submit`.

## Rollout Plan

1. Add generic request-context extraction, readable dispatch logs, and tenant on
   transition spans.
2. Keep persistence schema unchanged in this pass; durable provenance storage
   can be added by a later migration if Mission Control needs it outside
   Datadog.

## Consequences

Datadog becomes useful for runtime app traffic without requiring app authors to
write instrumentation code. Simulated usage remains real Temper app usage under
the runtime service, but Directed Evolution and other producers carry their
correlation data as metadata rather than as Temper core concepts.

## DST Compliance

The change only threads existing HTTP request metadata into telemetry. It does
not alter simulation-visible state transitions, scheduling, actor behavior, or
deterministic data structures.
