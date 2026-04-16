# ADR-0045: Raise WASM Integration Default Timeout to 120s

- Status: Accepted
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-wasm/src/types.rs` — `WasmResourceLimits::default()`
  - `crates/temper-server/src/state/dispatch/wasm.rs` — integration dispatch fallback

## Context

The WASM integration dispatcher applies a 30-second wall-clock timeout to any integration whose spec does not explicitly set `timeout_secs`. This default is enforced by wasmtime epoch interrupts and produced the error `execution timeout -- module exceeded time budget of 30s`.

For paw-agent sessions, only 3 of 12 integrations set `timeout_secs` explicitly (`call_llm: 600`, `run_tools: 900`, `compact_context: 120`). The remaining 9 inherit the 30s default. Under moderate orchestrator load, integrations such as `provision_workspace` and `check_steering` routinely cross 30s (HTTP fan-out, cold caches, LLM-fronted helpers) and trip the timeout. In paw-foresight meta-improvement runs, 30 of the last 50 orchestrator sessions failed with this exact error, corrupting the evaluation signal.

The root cause is that 30s is too aggressive for any integration performing outbound HTTP work, and the platform gave no feedback when an app relied on the default.

## Decision

### Sub-Decision 1: Raise the default to 120s

Change `WasmResourceLimits::default().max_duration` from `Duration::from_secs(30)` to `Duration::from_secs(120)`. Change the per-integration fallback in `crates/temper-server/src/state/dispatch/wasm.rs` that reads `timeout_secs` from integration config to unwrap to the same 120s value.

**Why this approach.** 120s covers the common case (HTTP fan-out + LLM-fronted helpers without streaming) while still being short enough that a stuck integration does not hold an actor's mailbox for minutes. Apps that need more can set `timeout_secs` explicitly; apps that want less can also set it. The change is strictly more permissive than today — it cannot regress any integration that was succeeding at 30s.

### Sub-Decision 2: Warn and count every default-fallback use

Emit a `tracing::warn!` in the dispatcher whenever an integration config omits `timeout_secs`, including tenant, entity type, and module name. Emit a counter `temper_wasm_integration_default_timeout_used_total{module,tenant,entity_type}` at the same site. Tag the dispatch span with `wasm.timeout_source = explicit | default`.

**Why this approach.** The warning makes the condition debuggable; the counter makes it alertable. Apps with integrations falling back many times per day should wire `timeout_secs` explicitly rather than relying on a platform default. This closes the observability gap without forcing anyone's hand.

## Rollout Plan

1. **Phase 0 (this PR)** — Default raise + warn + counter + span attribute. Ships with unit coverage.
2. **Phase 1 (follow-up, not in scope)** — App-level defaults in `app.toml` so an app can set one timeout for all its integrations without editing every integration spec.

## Consequences

### Positive
- Eliminates the 30s-timeout failure mode for paw-agent and every other app that relies on the default.
- Surfaces configuration gaps through logs and metrics instead of silent failures.

### Negative
- Actor mailboxes can now be held for up to 120s by a runaway integration instead of 30s. Mitigated: mailbox slots are a bounded resource per actor; the bound itself is unchanged, only the duration any slot can be held.

### Risks
- An integration that was masking a real bug by timing out at 30s will now wait 4× longer before failing. Mitigation: the warning makes default-timeout usage visible, so operators can tighten timeouts once they know the apps that need it.

### DST Compliance
- `WasmResourceLimits` lives in `temper-wasm`, not a simulation-visible crate. The dispatcher site that reads `timeout_secs` is in `temper-server`; the change is a config-default edit and adds observability-only side effects. No new `chrono`, `std::thread`, or non-deterministic primitives. The counter increment uses the existing OpenTelemetry global meter — same pattern as every other metric in `runtime_metrics.rs`.

## Non-Goals

- Adding a global ceiling on `timeout_secs` — app authors remain responsible for their own timeout discipline.
- Dynamic timeout computation (e.g., per-request sizing) — out of scope; use explicit `timeout_secs`.

## Alternatives Considered

1. **Keep 30s, require every integration to set `timeout_secs` explicitly.** Rejected — breaks every existing app whose spec omits the key; a flag-day change with no rollout story.
2. **Raise to 300s.** Rejected — too lenient for integrations that never intentionally run that long; a stuck integration would tie up a mailbox slot for five minutes.
3. **Set the default per-integration-kind (e.g., 60s for pure-compute, 180s for HTTP).** Rejected — adds a classification dimension that apps would have to learn. The simpler knob (one default, override when needed) is sufficient.

## Rollback Policy

Revert the two-line default change in `types.rs` and `wasm.rs` and remove the counter/warning wiring. The counter would stop emitting at that point but leave no persistent artifact.
