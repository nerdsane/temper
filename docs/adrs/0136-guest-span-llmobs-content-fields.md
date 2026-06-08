# ADR-0136: Guest Span LLMObs Content Fields

- Status: Accepted
- Date: 2026-06-08
- Deciders: Temper core maintainers
- Related:
  - ADR-0096: WASM guest observability API adoption
  - `crates/temper-wasm/src/engine/guest_spans.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

Temper guest spans can attach arbitrary OpenTelemetry attributes, but the
Datadog trace pipeline reliably facets and renders only attributes that are
also recorded on static `tracing` span fields. The existing LLM span field
surface covered short metadata such as `gen_ai.provider.name` and
`gen_ai.request.model`, but omitted content-bearing fields such as
`gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.prompt`, and
`gen_ai.completion`.

In TemperPaw production this created LLM spans that appeared in Datadog with
provider/model metadata but rendered as `No content`.

## Decision

Temper guest spans and host HTTP span-hint spans now declare the GenAI LLMObs
content fields as static tracing fields and include them in
`datadog_visible_span_hint_field`.

The added visible field surface covers:

- request/response model metadata
- conversation id and system fields
- input, output, prompt, and completion content attributes
- finish reasons and token usage
- response body size

The values continue to flow through the existing attribute sanitization and
UTF-8-safe truncation path before being recorded.

## Consequences

### Positive

- Datadog can receive and render LLM content attributes on guest spans instead
  of only short metadata.
- Existing guest-span and header-span APIs keep the same caller contract.
- Oversized values remain bounded by the host span-attribute truncation limit.

### Negative

- LLM content fields become a larger static span field surface.
- Operators must continue to avoid sending secrets or unbounded raw artifacts
  as LLM span attributes.

### Risks

- Datadog could still drop content if caller payloads are too large before they
  reach the host. TemperPaw mitigates this by bounding provider completion
  attributes before the host call.

## Non-Goals

- This does not introduce a separate LLM observability bridge.
- This does not change the guest-span host API shape.

## Rollback Policy

Remove the added static fields and allowlist entries, then repin downstream
applications to the rollback commit.
