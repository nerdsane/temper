# ADR-0054: Log Standard — JSON, Trace-Correlated, Level-Disciplined

- Status: Proposed
- Date: 2026-04-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0053: Datadog service decoupling
  - ADR-0055: Continuous profiling
  - `crates/temper-server/src/logging.rs` (to be created)
  - `crates/temper-server/src/main.rs` (logging init)

## Context

Temper logs exist and were load-bearing during the 2026-04-18 Katagami incident investigation — we grep'd them to diagnose workspace_provisioner failures and Session TimeoutFail events. The experience surfaced three concrete problems:

1. **No declared schema.** Logs mix free-form text (`"build_session_message: created Session '...'"`), pseudo-structured prefixes (`"tool dispatch complete tool_name=temper.read ..."`), and the occasional JSON blob. Datadog facets are not registered; `@tenant`, `@entity_id`, `@trace_id` are not guaranteed to be present or in a known place.
2. **No trace correlation.** Log lines emitted during a span do not carry the `trace_id` / `span_id`. The Datadog UI's "logs from this trace" feature is dark. Correlating a failing span with its log output requires manual timestamp alignment.
3. **`warn` is overused.** The audit counted 613 `WASM integration falling back to default timeout — wire timeout_secs explicitly` warnings in a 45-minute window. None are actionable to the on-call engineer. Real warnings drown in noise.

The failure pattern is clear: logs are the highest-cardinality observability signal we have, and we are the platform that lets every app generate them, but we have not decided what they look like. This ADR fixes that.

## Decision

All logs emit JSON with a fixed schema, automatically correlated with the active tracing span, processed by a dedicated Datadog pipeline, and classified by a strict level policy.

### Sub-Decision 1: Fixed JSON schema

Every log line is a JSON object with these fields:

```
{
  "timestamp": "2026-04-18T13:00:00.000Z",   // ISO-8601 UTC, ms precision
  "level": "info" | "warn" | "error" | "debug",
  "service": "temper" | "openpaw",
  "embodiment": "openpaw" | "tamago" | ...,
  "tenant": "default" | ...,                  // when in scope
  "entity_type": "Session" | "File" | ...,    // when in scope
  "entity_id": "ss-..." | ...,                // when in scope
  "action_name": "Configure" | ...,           // when in scope
  "trace_id": "0af7651916cd43dd8448eb211c80319c",  // when inside a span
  "span_id": "b7ad6b7169203331",                    // when inside a span
  "message": "free-form message text",
  "error": {                                  // only on level:error
    "kind": "ActionRejected" | ...,           // stable identifier
    "message": "user-safe message",
    "source": "actor dispatch exhausted after 3 attempt(s)"
  },
  "extra": { ... }                            // structured key-values specific to the event
}
```

**Why all-required-when-in-scope**: downstream aggregation is valuable only if fields exist on every line where they apply. If `tenant` is "usually there", Datadog facet queries are unreliable. `tracing::Span` context populates the scoped fields automatically; manual emissions use `tracing::info!` macros that pick them up via the span registry.

### Sub-Decision 2: `tracing-opentelemetry` for trace correlation

The OTel layer (already present, see ADR-0053's `otel.rs` scope) extends the tracing subscriber to inject `trace_id` and `span_id` into every log line emitted within a span. This is the built-in integration — no custom glue.

Verified by: start a trace, emit `tracing::info!("inside span")`, observe `trace_id` and `span_id` in the JSON output matching the active span.

**Consequence**: Datadog UI's "View logs for this trace" button works.

### Sub-Decision 3: Log level policy (strict)

- **`error`**: user-visible failure that would page someone. Must include `error.kind` and `error.message`. Example: `actor dispatch exhausted after 3 attempt(s)`.
- **`warn`**: retry-recoverable or degraded-path condition that *could* escalate and wants operator attention. Example: `dispatch retry succeeded after 2 attempts`. Not: 613× `falling back to default timeout`.
- **`info`**: meaningful state transition, external I/O boundary, spec reload, session creation. One-line summaries, not debug dumps.
- **`debug`**: internal state, parameter echoes, small-scale development aids. Disabled in prod via `RUST_LOG=info,temper=info`.

Existing `warn`s must be audited when this ADR lands. Candidates for downgrade to `info` or `debug`:
- `WASM integration falling back to default timeout` (613/45m; this is a configuration observation, not a failure).
- `liveness coverage missing (ADR-0050)` (444/45m during rollout; warn only at spec load, not per action).
- `tool dispatch complete ... success=false result_preview=... HTTP 403 ...` (genuine upstream 4xx from the tool target; `info` at most).
- `provider selection: provider=anthropic has no key, falling back to openai_codex` (config observation at boot; emit once, not per call).

A small PR lands as part of this ADR's implementation that does the audit pass.

### Sub-Decision 4: Datadog log pipeline

A pipeline named `OpenPaw / Temper Logs` is created in Datadog and its JSON definition is checked into `/Users/seshendranalla/Development/openpaw/dd-pipelines/temper-openpaw.json`.

Pipeline steps:

1. **Parse JSON** — extract nested fields.
2. **Remap service** to `@service` standard attribute.
3. **Extract facets** — `@tenant`, `@entity_type`, `@entity_id`, `@action_name`, `@trace_id`, `@span_id`, `@error.kind`, `@embodiment`. Registered as official Datadog facets (searchable dropdowns in Log Explorer).
4. **Sensitive-data scanner rules** — redact emails, API tokens (any string matching `sk-...`, `gho_...`, `xoxb-...`), session cookies. Applied at pipeline ingest, so redaction is enforced even if application code accidentally logs secrets.
5. **Forward to index**: platform logs (`service:temper`) to a 30-day hot index. Embodiment logs (`service:openpaw`) to the same 30-day index. Everything archived to S3 indefinitely.

### Sub-Decision 5: Log-based metrics

Datadog's log-based metrics generate custom metrics from log patterns without code changes. We generate:

- `openpaw.logs.errors` — count of `level:error`, tagged `service`, `tenant`, `error.kind`. Enables "error rate spiked" alerts without requiring a bespoke Rust metric for every failure mode.
- `openpaw.logs.wasm.default_timeout_fallback` — count of the specific WASM warning pattern. Even after we demote the log to `info`, the count remains observable.
- `openpaw.logs.warns` — count of `level:warn`, tagged `service`. Low-priority dashboard signal.

Log-based metrics are billed differently from custom metrics; we stay under Datadog's free tier by picking 3 generators at launch.

### Sub-Decision 6: Rust implementation

- `crates/temper-server/src/logging.rs` — new module, centralized init.
- `tracing-subscriber` with the JSON formatter: `tracing_subscriber::fmt::layer().json().with_timer(UtcTime::rfc_3339()).with_target(false).with_current_span(false).with_span_list(false)`.
- `tracing-opentelemetry` layer composed on top for trace ID injection.
- Output: stdout (Railway picks it up natively). For local dev, `FOREGROUND_LOGS=pretty` opts into the compact formatter.
- `RUST_LOG` default: `info,temper=info,openpaw=info`.

### Sub-Decision 7: Schema evolution rules

- Adding a field: OK, non-breaking.
- Removing a field: ADR-level change. Datadog pipelines depend on field names.
- Renaming a field: same as remove + add. Avoid.
- Changing a field type: forbidden. Pipelines cannot handle type drift.

## Rollout Plan

1. **Phase 0 (Immediate)** — `logging.rs` lands with JSON formatter. Deploy. Verify log shape in Railway + Datadog.
2. **Phase 1 (+1 day)** — Trace correlation wired. Verify by clicking a span in Datadog APM and confirming logs appear.
3. **Phase 2 (+2 days)** — Datadog pipeline JSON committed and deployed. Facets registered.
4. **Phase 3 (+3 days)** — Warn-level audit PR: demote the five known-noisy warnings, add emit-once patterns for boot-time observations.
5. **Phase 4 (+5 days)** — Log-based metrics created. Monitors wired: `[Temper] Log Error Rate Spike`, `[Temper] Panics in logs`, `[OpenPaw] Webhook Receive Errors`.
6. **Phase 5 (+1 week)** — Retention policy tuned based on observed log volume.

## Readiness Gates

- JSON log shape verified against schema in Railway prod for 24 hours.
- Trace ID present on ≥95% of logs emitted during an active request flow.
- `openpaw.logs.errors` metric populated and surfaces on dashboard.
- Warn rate drops by ≥90% after Phase 3 audit.

## Consequences

### Positive
- Structured log search via Datadog facets — no more grep.
- Trace ↔ log correlation functional; incident investigation ~5× faster.
- Log-based alerting covers generic failure modes without requiring bespoke metrics.
- Secret redaction is centralized; less likely to leak.

### Negative
- Log volume briefly higher (JSON is more verbose than free text). Then drops sharply once warn-level audit lands.
- Developer friction: `tracing::info!("foo {}", x)` still works, but to get structured fields you write `tracing::info!(tenant = %t, "foo {}", x)`. Documented in the crate README.

### Risks
- **Accidental secret logging**. Rust has no type-level guarantee that a `String` is non-sensitive. Mitigation: Datadog scanner rules at ingest; periodic audit of `info!` call sites that pass large structs.
- **Log volume budget blown**. Mitigation: Phase 4 warn-audit plus flex-tier archiving; 30d hot index size capped.

### DST Compliance
- Logs are not part of DST simulation; tracing subscriber is excluded. No determinism impact.

## Non-Goals

- Structured-logging types (e.g., `slog`-style typed events). `tracing`'s key-value macros are enough.
- Per-request correlation IDs distinct from trace IDs. `trace_id` is the correlation ID.
- Log sampling (volume should not require it once warn-audit lands).
- Alternative backends (Loki, Elastic). Datadog-native per the parent audit plan.

## Alternatives Considered

1. **Keep free-form text, add parsing rules in Datadog** — rejected. Every log-emit site becomes a coupling point with pipeline regex. Fragile and slow to iterate.
2. **OTel logs API instead of tracing crate** — rejected for v1. The Rust OTel logs API is still young; `tracing` with the OTel layer covers 95% of what we need and is production-proven.
3. **Separate log service per embodiment** — rejected. Cross-embodiment incident analysis benefits from unified indexing.

## Rollback Policy

- Phase 0 reversal: swap JSON formatter for the compact formatter. One line change.
- Phase 2 reversal: pipeline archived; facets remain registered (harmless).
- Log-based metrics: deletable from Datadog UI without source changes.

Rollback is low-cost. The JSON schema is the only durable artifact.
