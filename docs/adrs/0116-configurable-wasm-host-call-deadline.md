# ADR-0116: Configurable WASM Host Call Deadline

- Status: Accepted
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0045: WASM default timeout
  - ADR-0086: WASM host boundary observability
  - `crates/temper-wasm/src/engine/host_functions.rs`
  - `crates/temper-wasm/src/engine/mod.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs`

## Context

WASM integrations already accept `timeout_secs` through integration config. The server dispatch path uses that value for both the `ProductionWasmHost` HTTP client timeout and `WasmResourceLimits::max_duration`.

The host-function bridge had an additional hardcoded 60-second outer deadline around asynchronous host calls. That deadline was added as a defensive guard so a hung host future cannot pin an entity actor indefinitely, but it accidentally became a lower timeout than the integration's configured budget. Long-running inference calls configured for 120s, 600s, or 900s can therefore fail at 60s even though the app explicitly requested a longer budget.

## Decision

The host-function outer deadline is part of the invocation budget, not a platform-wide constant. Each WASM invocation records a host-call deadline derived from `WasmResourceLimits::max_duration`. Host functions use the remaining invocation budget when bridging asynchronous work from the synchronous Wasmtime ABI.

This preserves the defensive guard while allowing app-level integration budgets to control legitimate long-running provider calls. If an invocation has already exhausted its budget before entering a host call, the host call receives a minimal deadline and returns an error promptly.

## Rollout Plan

1. **Phase 0 (Immediate)** — Carry the per-invocation deadline in `HostState`, update `host_http_call`, `host_connect_call`, and `host_http_call_stream` to use the remaining budget, and add regression coverage for slow host calls.
2. **Phase 1 (TemperPaw)** — Update TemperPaw to depend on the Temper commit that contains this fix. Existing `timeout_secs` values then take effect without changing TemperPaw orchestration.

## Consequences

### Positive

- Long-running inference integrations can use their configured budgets instead of being clipped at 60 seconds.
- Hung host calls remain bounded by the same invocation budget used by the WASM engine.
- The host boundary now has one source of truth for duration.

### Negative

- A module with multiple sequential host calls consumes the same overall invocation budget. Later calls may receive less time than earlier calls.

### Risks

- Existing integrations that implicitly relied on the 60-second host boundary may now run longer when their configured or default budget is longer. This is intentional and observable through existing WASM invocation telemetry.

### DST Compliance

- This touches `temper-wasm`, outside the simulation-visible crates named by the DST guard. It uses `std::time::Instant` only inside the WASM sandbox execution path, matching existing wall-clock timeout code.

## Non-Goals

- This ADR does not add per-host-function timeout config separate from the invocation budget.
- This ADR does not change default `WasmResourceLimits`.

## Alternatives Considered

1. **Raise the constant to 600 seconds** — Rejected because it would keep two competing timeout sources and would still be wrong for shorter or longer integrations.
2. **Remove the outer host-call guard** — Rejected because the guard protects actors from hung host futures that the HTTP client timeout does not resolve.
3. **Add a separate `host_call_timeout_secs` config** — Rejected for this fix because the existing integration `timeout_secs` is already the user-facing budget. A separate knob can be added later if a concrete use case needs it.

## Rollback Policy

Revert the code changes and TemperPaw dependency update. The platform will return to the previous defensive 60-second host-call boundary.
