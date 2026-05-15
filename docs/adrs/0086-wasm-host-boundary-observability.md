# ADR-0086: WASM Host Boundary Observability

- Status: Accepted
- Date: 2026-05-13
- Deciders: Temper maintainers

## Context

TemperPaw depends on Temper WASM integrations for agent, file, blob, HTTP, and provider work. Datadog APM cannot see inside guest WASM code unless the host deliberately emits telemetry at the ABI boundary. Before this decision, outbound HTTP and streaming HTTP calls were visible, and guest logs were trace-correlated, but other meaningful host boundaries were too opaque:

- secret lookup
- spec evaluation
- guest progress emission
- cache reads/writes
- entity field reads, including blob-ref resolution
- stream hashing
- direct HttpEndpoint WASM dispatch context

That left agent-session traces with useful outer spans but insufficient detail when a guest stalled or read unexpected host data.

## Decision

Temper records first-class host-boundary telemetry for WASM invocations:

1. Latency-bearing host boundaries emit child spans under the active `wasm.invoke` span: `wasm.host.get_secret`, `wasm.host.evaluate_spec`, `wasm.host.connect_call`, `wasm.host.cache_contains`, `wasm.host.cache_to_stream`, `wasm.host.cache_from_stream`, `wasm.host.read_field`, and `wasm.host.hash_stream`, in addition to existing HTTP spans.
2. High-frequency guest progress remains an event, not a span. `wasm_guest.progress` is emitted as a Datadog-searchable log event and an OpenTelemetry span event on the active trace. This preserves chronological evidence without flooding traces with tiny progress spans.
3. Guest logs and progress events carry Datadog-readable correlation fields: `trace_id`, `span_id`, `dd.trace_id`, `dd.span_id`, `tenant`, `entity_type`, `entity_id`, `action_name`, `trigger_action`, `session_id`, `agent_id`, `wasm_module`, and workflow root/run fields when present.
4. `WasmInvocationContext` includes `wasm_module`; the SDK exposes it to guests, and server dispatch/direct/HttpEndpoint paths populate it.
5. Secret lookup spans may record the secret key name but never the secret value.

## Consequences

Humans and agents can open a TemperPaw trace and see which WASM module ran, which host functions it used, and where host-side time was spent across HTTP, blob/R2/file, cache, field-read, hash, secret, spec, and progress boundaries.

Progress is intentionally modeled as events rather than spans. If a future guest needs custom nested timing inside WASM logic that is not represented by host boundaries, Temper should add an explicit guest-to-host span API (`host_start_span`, `host_end_span`, `host_add_span_event`) rather than relying on ad hoc logging conventions.

## Verification

- Red tests added first for missing secret/progress observability:
  - `secret_lookup_emits_contextual_host_span_without_secret_value`
  - `progress_emission_creates_correlated_guest_event`
- Green verification:
  - `cargo test -p temper-wasm -- --nocapture`
  - `cargo test -p temper-server wasm -- --nocapture`

