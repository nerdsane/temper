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
not opted in, the sensitive `gen_ai.*` content attributes and captures are
filtered out of the hints immediately after parsing, before they are recorded
onto either the tracing span or the raw OTel span. The decision is propagated
into `ProductionWasmHost` via a new `with_llm_content_export(bool)` builder that
**defaults to `false` (redact)**, set from `ServerState::export_llm_content` at
every server-side host construction site.

Sensitive content attrs: `gen_ai.input.messages`, `gen_ai.prompt`,
`gen_ai.system_instructions`, `gen_ai.output.messages`, `gen_ai.completion`,
`gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`.

## Consequences

### Positive
- No tenant's LLM prompts/completions reach Datadog unless that tenant opts in.
- Redaction is fail-safe: any host built without the explicit opt-in redacts.
- Metadata-based dashboards (tokens, latency, model, provider) keep working for
  every tenant.

### Negative
- Tenants currently relying on content in Datadog must be added to
  `TEMPER_LLM_CONTENT_EXPORT_TENANTS` to keep it. This is the intended,
  deliberate default flip.
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
  builder default makes new paths secure automatically.

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
   telemetry bootstrap. Rejected as riskier than gating at the two known sources.
3. **Export by default, opt-out** — Rejected: the finding is that content leaks
   by default. Only redact-by-default closes it.

## Rollback Policy

Set `TEMPER_LLM_CONTENT_EXPORT_TENANTS=*` to restore the previous
export-everything behavior for all tenants, or revert this change set — the
redaction helpers are additive and self-contained.
