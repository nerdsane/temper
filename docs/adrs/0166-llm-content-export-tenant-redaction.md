# ADR-0166: Per-Tenant Redaction of LLM Observability Content

- Status: Accepted
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0037: `X-Temper-Span-*` header hints (host HTTP capture path)
  - `crates/temper-server/src/state/dispatch/wasm.rs` (LLM dispatch recording)
  - `crates/temper-wasm/src/host_trait/span_hints.rs` (host HTTP capture)
  - `crates/temper-observe/src/wide_event/agent.rs` (wide-event builders)

## Context

When an entity integration is marked `llm = true`, the WASM dispatch path records
the model's **prompt, completion, and system instructions** onto the OpenTelemetry
span, into the WideEvent, and submits them to Datadog LLM Observability. A second,
independent path lets WASM modules (e.g. `llm_caller`) capture request/response
**content** onto the host `wasm.host.http_call` span via `X-Temper-Span-Attr-*`
and `X-Temper-Span-Capture-Response-*` headers (ADR-0037).

Both paths export raw LLM content to the telemetry backend **unconditionally** —
there is no per-tenant control. A tenant whose prompts or completions contain
PII, secrets, or regulated data has no way to keep that content out of Datadog.
This is the ARN-243 finding: LLM observability leaks full prompts with no tenant
opt-out.

The existing `strip_private_observability_params` helper removes `_gen_ai_*`
content keys **before persistence** (so they are not echoed back to the guest or
stored in entity state), but the telemetry sinks read those keys *before* that
strip runs — so the content still reaches Datadog.

## Decision

Gate all LLM content export on a per-tenant policy that **defaults to redact**.
Content is only exported for tenants that explicitly opt in. Metadata
(token counts, model, provider, finish reason, trace-linking IDs) is always
exported — only prompt/completion/system/tool content is redacted.

### Sub-Decision 1: Per-tenant opt-in resolved on `ServerState`

`ServerState` gains an immutable `llm_content_export_tenants: Arc<BTreeSet<String>>`
loaded once at startup from `TEMPER_LLM_CONTENT_EXPORT_TENANTS` (comma-separated
tenant ids; `*` opts in every tenant). The resolver is a pure set lookup:

```rust
pub fn export_llm_content(&self, tenant: &str) -> bool {
    self.llm_content_export_tenants.contains("*")
        || self.llm_content_export_tenants.contains(tenant)
}
```

An empty set (the default) means every tenant is redacted.

**Why this approach**: it is redact-by-default, per-tenant, deterministic (read
once at startup, then a pure lookup on the hot path), and requires no new storage
dependency or per-dispatch async. It mirrors the existing `local_tdata_hosts`
env-loaded allowlist. A future enhancement can move the opt-in into per-tenant
Cedar policy or the secrets vault without changing the redaction sites.

### Sub-Decision 2: Dispatch path — strip content keys from callback params

Four telemetry sinks (span record, `llm_call_wide_event`, `submit_llmobs_llm_span`,
`submit_llmobs_tool_spans`) all read content from the same `result.callback_params`
map. A single strip of the content keys — `_gen_ai_input_messages`,
`_gen_ai_output_messages`, `_gen_ai_system_instructions`, `_dd_llmobs_tool_spans` —
before the sinks run redacts all four at once. Metadata keys (`_gen_ai_provider`,
`_gen_ai_model`, `_gen_ai_finish_reason`, `_gen_ai_*_span_id`, token counts) are
preserved.

**Why this approach**: one choke point covers span, WideEvent, and both LLM-Obs
submissions. It cannot be bypassed by adding a new sink downstream, because the
content is already gone from the map.

### Sub-Decision 3: Host capture path — filter span hints before applying

The host HTTP capture path builds a `SpanHints` struct from guest headers, then
applies its attributes and response captures onto the span. When the tenant is
not opted in, the hints are filtered immediately after parsing, before they are
recorded onto either the tracing span or the raw OTel span. The decision is
propagated into `ProductionWasmHost` via a new `with_llm_content_export(bool)`
builder that **defaults to `false` (redact)**, set from
`ServerState::export_llm_content` at every server-side host construction site.

This filter is an **allowlist, not a denylist**, because the thing it gates is
the thing that names the data. Both halves of a span hint are guest-controlled:
the attribute name arrives as an `X-Temper-Span-Attr-*` header, and a response
capture is a `(name, json_pointer)` pair whose value is lifted out of the
provider's response body by that pointer. Enumerating the canonical `gen_ai.*`
content keys therefore stops only a module that uses the canonical names — a
module that sends `X-Temper-Span-Capture-Response-llm.response.text:
/content/0/text` exports the same completion under a name no denylist carries.

So, for a tenant that has not opted in, one rule applies at every point an
untrusted guest names telemetry — span hints, guest spans, guest span events, and
guest wide events:

- **Attributes inside the `gen_ai.*` namespace** survive only if the key is a
  recognised metadata key (model, provider, token counts, finish reasons,
  conversation/tool ids, temperature, max tokens), and their values are clamped
  to 256 bytes. The clamp is what makes the allowlist mean anything: a key name
  cannot turn an untrusted value into metadata, so without a bound a module sends
  the whole prompt as `gen_ai.request.model`. Every legitimate metadata value is
  far shorter than the bound.
- **Attributes outside the namespace** pass through unchanged. These channels are
  the generic observability ABI, not LLM-only ones — see Scope below for why
  redacting them would cost working capability without buying protection.
- **Response captures** are dropped in full. Every capture is by construction a
  value read out of the response body via a guest-supplied pointer, so the
  attribute name says nothing about whether the value is content.
- **The guest-supplied span name** is clamped to the same 256 bytes, since it is
  free text on the same channel.

The decision reaches the guest-facing APIs through `WasmHost::exports_llm_content`
(default `false`), which the engine reads when it builds the guest span registry.

Content attrs (exported only on opt-in): `gen_ai.input.messages`,
`gen_ai.prompt`, `gen_ai.system_instructions`, `gen_ai.output.messages`,
`gen_ai.completion`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`.

The cost of the allowlist is that a new legitimate metadata attribute is redacted
for non-opted-in tenants until it is added to `is_llm_metadata_attr`. That is the
correct direction to fail: a missing dashboard field is recoverable, an exported
prompt is not.

### Sub-Decision 4: Test the policy, not only the filter

The filter is only as good as the predicate that opens it, so
`export_llm_content` is split into pure functions
(`parse_llm_content_export_tenants`, `tenant_exports_llm_content`) and tested
directly: unset/blank input opts in nobody, listed tenants match exactly (no
prefix, no case folding), and `*` opts in everyone. Each was mutation-checked —
default-allow, prefix matching, and keeping blank entries each turn the suite
red. Without these, a policy flipped to default-allow would leak every tenant's
prompts while every redaction test stayed green.

## Consequences

### Positive
- For a tenant that has not opted in, no LLM content reaches the telemetry
  backend **under the `gen_ai.*` semantic-convention names** — the names LLM
  Observability, the GenAI dashboards, and the wide-event pipeline actually read.
  That holds across all four channels an untrusted guest can reach: the host HTTP
  span-hint path, the callback params, the guest manual-span API
  (`host_start_span` / `host_set_span_attributes` / `host_add_span_event`), and
  `host_emit_wide_event`.
- Redaction is fail-safe: `WasmHost::exports_llm_content` defaults to `false`, so
  a host that never answers redacts, and the builder default matches.
- Metadata-based dashboards (tokens, latency, model, provider) keep working for
  every tenant, and non-LLM guest diagnostics (`provider.request_id`,
  `rpc.method`, application attributes) are untouched.

### Measured reach of the span-hint gate

A behavioral test (`crates/temper-wasm/tests/span_hint_redaction_behavior.rs`)
drives a real `http_call` and reads the span back out of an in-memory OTel
exporter, rather than trusting the code shape. It showed something the unit tests
could not: `apply_span_hints` sets non-Datadog-visible attributes only via
`otel_span.set_attribute` on a context-derived handle, which does not propagate
to the exported span. So on that channel, only names in
`datadog_visible_span_hint_field` reach the backend at all today.

Two consequences, both stated rather than assumed. The gate's real reach on the
span-hint path is narrower than its code suggests — it can only matter for names
the export path keeps. And guest diagnostics outside that set are being silently
dropped, which is an observability bug (tracked as **ARN-350**), not a security
one. The redaction is written for the wider surface deliberately: ARN-350 will
widen what reaches the exporter, and the gate must already be correct when it
does. The behavioral test asserts today's narrower behavior explicitly so that
fixing ARN-350 forces a revisit here instead of quietly changing the surface.

### Scope — what this does *not* claim

Stated plainly, because an overstated gate is worse than a narrow one: this does
not make it impossible for a determined guest module to move bytes into
telemetry. A module already holds its own prompt, and several channels carry
free-form text that carries no agreed meaning to redact against:

- `host_log` and `log_structured` attach the guest's message to span events and
  tracing events verbatim; a module that logs its prompt exports it.
- Guest-supplied error strings reach `error.message` / `exception.message`.
- Attributes outside the `gen_ai.*` namespace — on span hints, guest spans, and
  wide events — are the module's own application telemetry and pass through.

Closing those would mean disabling guest logging and guest observability
outright, which removes working capability for every tenant to stop a guest from
exporting data it already owns. The line drawn here is: **the semantic-convention
namespace that downstream systems interpret as LLM content is governed; free-form
guest text is not.** Tracked as ARN-349 so the choice is revisited deliberately
rather than forgotten.

### Negative
- Tenants currently relying on content in Datadog must be added to
  `TEMPER_LLM_CONTENT_EXPORT_TENANTS` to keep it. This is the intended,
  deliberate default flip.
- Response captures (`X-Temper-Span-Capture-Response-*`) are dropped entirely for
  non-opted-in tenants, including one that points at a metadata field such as
  token counts. This is a real, if narrow, capability change: a capture's value
  comes from a guest-supplied JSON pointer into the provider's response body, so
  the attribute name cannot establish that the value is metadata — a capture
  named `gen_ai.usage.input_tokens` pointed at `/content/0/text` is a completion.
  Metadata that used to arrive by capture should arrive as a span attribute
  instead, which the module can set from its own parsed response.
- Tool-call observability (`_dd_llmobs_tool_spans`) bundles tool arguments and
  results (content) together with tool name and duration (metadata) in a single
  array. For non-opted-in tenants the whole array is suppressed rather than
  field-redacted, so tool-level metadata is lost too. This errs toward less
  export, which is the safe direction for a security default; a future
  refinement can field-redact within each tool span if the metadata proves
  valuable on its own.

### Risks
- A future LLM export path built through an untouched host would redact for
  opted-in tenants (a false redaction, never a leak) — acceptable, and the
  trait default makes new paths secure automatically. Note this cuts both ways.
  Production builds a three-layer host —
  `AuthorizedWasmHost(LocalTDataWasmHost(ProductionWasmHost))` — and only the
  innermost layer holds the flag, so **every** wrapper has to forward
  `exports_llm_content`. A wrapper that does not makes the engine read the trait
  default and redact even for an opted-in tenant: fail-safe, but the opt-in is
  inert on that channel. Both wrappers forward, and the test asserts on the real
  three-layer stack — an earlier version wrapped `ProductionWasmHost` directly,
  a composition production never builds, and it passed while the real chain
  dropped the decision.
- The `gen_ai.*` metadata allowlist is a list, and lists go stale. A new
  legitimate metadata key is redacted for non-opted-in tenants until it is added
  to `is_llm_metadata_attr`. That is the correct direction to fail — a missing
  dashboard field is recoverable, an exported prompt is not — but it will look
  like a bug to whoever adds the key.

### DST Compliance
- `TEMPER_LLM_CONTENT_EXPORT_TENANTS` is read once at `ServerState`
  construction (`// determinism-ok: read once at startup`), matching the
  existing `env_local_tdata_hosts` pattern. `export_llm_content` is a pure
  set lookup. The strip and hint-filter helpers are pure, order-independent
  transforms over `BTreeSet`/`Vec` — no wall clock, RNG, or ambient I/O.

## Non-Goals
- Field-level or regex-based redaction of *parts* of a prompt. This is an
  all-or-nothing content gate per tenant.
- A runtime admin API to toggle a tenant's opt-in. Startup env config only for now.

## Alternatives Considered

1. **Cedar policy per tenant** — Model export as a Cedar action evaluated per
   dispatch. Rejected for now: heavier (new action/resource, per-call evaluation
   on a hot path) than the finding requires. Left as a documented follow-up.
2. **OTel SpanProcessor that strips content at export** — One central place, but
   requires the tenant on every span and non-trivial processor plumbing in the
   telemetry bootstrap. It is, however, the only shape that would cover the
   free-form channels named under Scope, and is the natural next step if the
   residual there proves unacceptable.
3. **Export by default, opt-out** — Rejected: the finding is that content leaks
   by default. Only redact-by-default closes it.

## Rollback Policy

Set `TEMPER_LLM_CONTENT_EXPORT_TENANTS=*` to restore the previous
export-everything behavior for all tenants, or revert this change set — the
redaction helpers are additive and self-contained.
