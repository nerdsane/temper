# ADR-0103: Local TData Host Stream Contract Preservation

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0099: Local WASM TData Host Path
  - ADR-0102: Local TData Public Origin Bypass
  - ADR-0100: WASM Invocation Phase Observability
  - `crates/temper-server/src/state/dispatch/wasm/local_tdata_host.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

ADR-0099 introduced `LocalTDataWasmHost` as a server-side wrapper around the
production WASM host. Its mission is deliberately narrow: intercept eligible
textual `GET` and `POST` calls to local `/tdata` and run the existing OData
handlers in-process, while every non-local capability continues through the
wrapped host.

Production Datadog evidence from May 17-18, 2026 shows that the wrapper did
not preserve the full `WasmHost` contract. Codex provider calls reached the
streaming boundary after successful auth refresh and context preparation, then
failed before the outbound OpenAI Codex HTTP stream span was created:

```text
http_stream::streaming_call: begin_outbound rc -4
```

The first bad TemperPaw deployment was `d230d3c...`, which rolled Temper from
last-good `ed3d0c...` to `6d09be...`. That Temper commit added
`LocalTDataWasmHost`. The wrapper delegated `http_call`, `http_call_binary`,
`connect_call`, secrets, logging, and progress, but not the `http_stream_*`
methods. Because the `WasmHost` trait provides default "not supported" stream
methods, the wrapper became a capability sink for streaming calls even though
the inner `ProductionWasmHost` supported them.

This is a host-contract bug, not a provider, prompt, Discord, or OpenAI API
behavior issue.

## Decision

`LocalTDataWasmHost` must be a transparent wrapper for every `WasmHost`
capability it does not intentionally intercept.

### Sub-Decision 1: Forward All Streaming Methods

The wrapper will explicitly delegate:

- `http_stream_begin_outbound`
- `http_stream_read`
- `http_stream_read_bounded`
- `http_stream_try_write`
- `http_stream_close`
- `http_stream_response_head`
- `http_stream_send_response_head`

**Why this approach**: local TData routing optimizes only textual local OData
calls. Streaming provider calls, inbound streaming responses, sandbox streams,
and future streaming users must keep the exact production host behavior.

### Sub-Decision 2: Forward Remaining Non-TData Host Capabilities

The wrapper will also delegate the structured observability host methods that
were added after the original wrapper shape:

- `emit_wide_event`
- `log_structured`
- `emit_metric`

**Why this approach**: wrapper code must not silently inherit "not supported"
defaults for host features it does not own. A local transport optimization
should not reduce guest observability or diagnostics.

### Sub-Decision 3: Add Regression Coverage At The Wrapper Boundary

The test suite will include a host double that proves stream begin, write,
read, response-head, response-head-send, and close all reach the delegate
through `LocalTDataWasmHost`.

**Why this approach**: the previous local TData tests proved interception and
delegation for regular HTTP calls, but did not prove that unrelated host
capabilities stayed intact. The regression test protects the exact failure
boundary Datadog exposed.

## Rollout Plan

1. **Phase 0 (Immediate)**: ship the wrapper forwarding patch and regression
   tests in Temper.
2. **Phase 1 (TemperPaw Bump)**: roll the fixed Temper revision into
   TemperPaw, preserving any production rollback that has already restored
   service health.
3. **Phase 2 (Production Proof)**: run a live Codex streaming request and
   confirm Datadog shows the normal outbound Codex stream span instead of
   `begin_outbound rc -4`.

## Readiness Gates

- Focused `local_tdata` tests prove regular local TData behavior still works.
- New stream delegation test proves all streaming methods forward through the
  wrapper.
- `temper-server` check, format, clippy, and relevant integration tests pass.
- The TemperPaw rollout passes CI/Docker before deployment.
- Live production proof shows Codex streaming reaches the outbound provider
  request path and does not emit the previous `rc -4` burst.

## Consequences

### Positive

- Restores Codex/OpenAI streaming through the local TData wrapper.
- Keeps the local TData latency win without making the wrapper a capability
  sink.
- Provides a reusable test pattern for future `WasmHost` wrappers.

### Negative

- Wrapper implementations remain somewhat verbose because Rust has no automatic
  "delegate every trait method" syntax.
- Future `WasmHost` trait additions still need reviewer attention unless we add
  a broader compile-time or test-time contract check.

### Risks

- Another future host method could be added and not forwarded. Mitigation:
  review wrapper diffs against the full `WasmHost` trait and prefer explicit
  regression tests when adding host capabilities.
- Production could still return `rc -4` for a different bridge failure after
  forwarding is fixed. Mitigation: use Datadog proof to distinguish wrapper
  failure from downstream stream-bridge/provider failure.

### DST Compliance

- This touches `temper-server`, a simulation-visible crate.
- The change only forwards calls to an existing delegate and does not add
  threads, wall-clock time, random IDs, filesystem access, network access beyond
  what the delegate already performed, or nondeterministic collection behavior.
- The added test double uses deterministic counters and fixed stream handles.

## Non-Goals

- Do not change the guest streaming ABI.
- Do not intercept streaming `/tdata` or File `$value` traffic locally.
- Do not change provider auth, OpenAI request construction, or retry policy.
- Do not tune latency further until production streaming health is restored.

## Alternatives Considered

1. **Disable `LocalTDataWasmHost` entirely**. This is a valid emergency rollback
   but gives up the measured local `/tdata` latency improvement instead of
   fixing the wrapper contract.
2. **Move local TData routing into `ProductionWasmHost` directly**. Rejected for
   this patch because it broadens the change and mixes server OData state into
   the generic WASM host.
3. **Rely on trait defaults for unsupported methods**. Rejected because this is
   exactly how the regression escaped: defaults are useful for simple hosts, but
   transparent wrappers must preserve supported inner capabilities.

## Rollback Policy

If the forwarding patch causes unexpected behavior, revert this ADR's code
change or temporarily remove the local TData wrapper wiring from WASM
invocation construction. Emergency production recovery can continue to use the
last-known-good TemperPaw version before `d230d3c...`.
