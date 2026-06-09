# ADR-0137: Datadog LLMObs Auto-Conversion Opt-Out

- Status: Accepted
- Date: 2026-06-09
- Deciders: Temper core maintainers
- Related:
  - ADR-0136: Guest Span LLMObs Content Fields
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-wasm/src/engine/guest_spans.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

ADR-0136 made LLM content attributes visible on Datadog-rendered guest spans.
That fixed spans where TemperPaw intentionally exported content, but production
also emits root OTel GenAI spans and provider-call helper spans whose purpose is
trace structure and provider metadata, not LLMObs transcript rendering.

Datadog can auto-convert GenAI OTel spans into LLMObs rows. When those
structure-only spans do not contain input or output content, the LLMObs UI
renders them as empty `No content` rows even though the span itself is valid
trace telemetry. Environment-level resource attributes are not sufficient for
this path because the opt-out attribute must be present on the spans Datadog is
evaluating.

## Decision

Temper root LLM spans and Datadog-visible WASM guest/host span hint surfaces
must support the `dd_llmobs_enabled` attribute.

The server records `dd_llmobs_enabled = false` on the root LLM span created for
provider-caller integrations. The WASM host declares `dd_llmobs_enabled` as a
static tracing field on guest spans and host spans that accept guest span hints,
and maps the corresponding `X-Temper-Span-Attr-dd_llmobs_enabled` hint to that
field.

Provider integrations that emit OTel GenAI helper spans can now set
`dd_llmobs_enabled=false` on those spans while still exporting normal trace
metadata. Integrations that intentionally emit complete LLMObs transcript spans
can continue to send content-bearing fields.

## Rollout Plan

1. **Phase 0 (Immediate)** - Ship the platform field and root-span opt-out,
   then repin TemperPaw so provider-caller spans send the new hint.
2. **Phase 1 (Production proof)** - Query Datadog after deploy and confirm new
   LLM span exports no longer include empty auto-converted rows.

## Consequences

### Positive

- Structure-only GenAI spans remain useful in traces without polluting LLMObs
  content tables.
- The opt-out follows the span instead of relying on process-wide environment
  metadata that may not be copied onto Datadog's auto-conversion input.
- The existing span-hint API shape remains unchanged.

### Negative

- Datadog-specific behavior is now represented in the platform span field
  surface.
- Callers must still choose intentionally whether a span is a transcript span
  or a structure-only helper span.

### Risks

- If a future Datadog exporter changes the opt-out attribute name, this field
  will need a compatibility update.
- A caller can still create empty LLMObs rows by emitting GenAI content spans
  without setting the opt-out or actual content.

### DST Compliance

- Changes touch `temper-server` and `temper-wasm`, but add only static span
  fields and deterministic attribute mapping.
- No wall-clock access, randomness, filesystem access, network I/O, or
  background scheduling is introduced.

## Non-Goals

- This does not disable Datadog tracing or APM ingestion.
- This does not remove content-bearing LLMObs spans that callers intentionally
  emit.
- This does not add a separate LLM observability bridge.

## Alternatives Considered

1. **Only use `OTEL_RESOURCE_ATTRIBUTES`** - Rejected because production spans
   continued to auto-convert without the opt-out appearing on the relevant
   spans.
2. **Remove GenAI attributes from helper spans** - Rejected because provider,
   model, token, and operation metadata are still useful trace context.
3. **Disable LLM observability globally** - Rejected because transcript-bearing
   spans should remain possible for integrations that provide complete bounded
   content.

## Rollback Policy

Remove the static field mapping and root-span opt-out, then repin downstream
applications to the rollback commit.
