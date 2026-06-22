# ADR-0152: Integration failure is never silent

- Status: Proposed
- Date: 2026-06-22
- Deciders: Temper core maintainers
- Supersedes: ADR-0140 (generalizes its composite-only propagation)
- Related:
  - ADR-0140: A Composite action fails when its integration fails (the
    composite-only special case this generalizes)
  - `crates/temper-server/src/state/dispatch/wasm/invocation_artifacts.rs`
    (`handle_wasm_failure` — propagates `Err` when no `on_failure`)
  - `crates/temper-server/src/state/dispatch/adapter.rs` (`handle_adapter_failure`)
  - `crates/temper-server/src/state/dispatch/mod.rs` (background dispatcher)
  - `crates/temper-server/src/state/dispatch/compensation.rs` (compensating dispatch)

## Context

A WASM or adapter integration that returns `success: false` (or traps) for an
action that declares **no `on_failure`** handler is silently swallowed.
`handle_wasm_failure` / `handle_adapter_failure` return `Ok(None)` when there is
no `on_failure`, and the dispatch loop reads `Ok(None)` as success. In background
mode (the OData default, `await_integration = false`) the dropped error never even
reaches a caller — the background dispatcher just `tracing::error!`s it
(`mod.rs`).

ADR-0140 closed this for **Composite** actions only, via
`composite_failure_must_propagate`. But the silent-drop class is not specific to
composites: any action whose integration is its real effect (a webhook that must
deliver, an adapter that must run a side effect) reports success while the effect
never happened. The general rule should be: an integration failure with no
declared recovery is never silent.

### The ordering constraint (why this is compensation, not rollback)

By the time integrations run, the action's parent state transition is **already
durable** — `run_post_dispatch_effects` runs after the actor commits the event.
Returning `Err` does not — and cannot — roll the transition back. ADR-0140 already
states this. So the only honest options on failure are:

1. **Inline mode**: return the `Err` to the caller as `success: false`. The
   transition stands, but the caller learns the integration failed and the
   protocol handler maps it onto the right status.
2. **Background mode**: dispatch a **compensating transition** on the source
   entity — a forward step that moves the entity to a failure state — because the
   original transition is durable and there is no caller to return to.

A true rollback is impossible; compensation is the correct primitive.

## Decision

### Sub-Decision 1: failure propagates when `on_failure` is absent

Remove `composite_failure_must_propagate` (the composite-only special case) and
make the failure handlers themselves the single source of truth.
`handle_wasm_failure` / `handle_adapter_failure` record the invocation, then:

- if `on_failure` is declared, run the author's recovery handler (unchanged —
  this includes `on_failure = "Fail"` pointing at a spec `Fail` transition);
- otherwise return `Err(error)` instead of the old `Ok(None)` swallow.

Both WASM integration-failure arms (clean `success: false` and host trap/error)
and the late-authz-denial arm all route through `handle_wasm_failure`, so the
propagation rule applies uniformly to **any** action — not just composites — and
the invocation telemetry is recorded before the `Err` is returned. The predicate
is just `on_failure.is_none()`, now expressed at the one site that owns the
decision rather than duplicated at each call site.

### Sub-Decision 2: inline failure becomes `success: false`

When `failure_must_propagate` is true and the dispatcher runs **inline**
(`await_integration = true`), the `Err` propagates out of
`dispatch_wasm_integrations_internal` / `dispatch_adapter_integrations_internal`
and `effects.rs` converts it to an `EntityResponse { success: false, error: ... }`
returned to the caller. This wiring already exists for the ADR-0140 composite
case; generalizing the predicate makes it apply to all integrations.

### Sub-Decision 3: background failure dispatches a deterministic compensation

When `failure_must_propagate` is true and the dispatcher runs in **background**
mode, the background spawn no longer drops the `Err`. Instead it dispatches a
deterministic **compensating transition** on the source entity, in this order:

1. the spec's declared `on_failure` (if present — but then
   `failure_must_propagate` is false and the internal dispatcher already ran it,
   so this case does not reach the compensation path);
2. else a declared `Fail` / error transition on the source entity, if one is
   enabled from the entity's current state;
3. else emit a **surfaced critical metric** (`temper_integration_failure_dropped_total`)
   **and** an Observe event (`integration_failure_dropped`) — never a silent drop.

The compensation dispatch routes through the sim-visible `dispatch_tenant_action`
path and uses only `sim_now()` / `sim_uuid()` — no wall clock, no random. The
candidate-transition lookup walks the spec's `TransitionTable` deterministically
(`BTreeMap` rule index).

### Sub-Decision 4: the `// determinism-ok` spawn boundary is respected

The background dispatcher runs inside `tokio::spawn` blocks explicitly marked
`// determinism-ok: async integration side-effects run outside the simulation
core`. That boundary stays. The *decision to compensate and the timing of the
spawn* live outside the deterministic sim core (there is no madsim/turmoil
executor; only the actor mailbox / `SimActorSystem` is replayed). What is
deterministic is the compensation itself once it reaches the mailbox: the chosen
`Fail` action, its params, and the `sim_now`/`sim_uuid` it stamps.

**Therefore the determinism guarantee is scoped honestly**: inline-mode
compensation ordering is seed-stable; background-mode compensation is tested for
*occurrence and correctness* (the entity reaches its `Fail` state, or the metric +
Observe event are emitted) — **not** for seed-stable ordering, because the trigger
runs in a `// determinism-ok` spawn outside the sim core.

## Alternatives Considered

- **Default `await_integration` to `true`** so every failure is inline. Rejected:
  this serializes the hot write path behind every integration round-trip,
  reintroducing exactly the latency the background mode exists to avoid.
- **Keep ADR-0140's composite-only rule.** Rejected: the silent-drop class is not
  composite-specific; a non-composite webhook/adapter whose effect is the action's
  real purpose drops just as silently.

## Known bound

The compensating `Fail` transition is dispatched in background mode and can
itself declare an integration. If that integration also fails with no
`on_failure`, compensation re-enters. Termination relies on **state
progression**: a `Fail` transition moves the entity into a terminal failure
state (e.g. `Failed`) from which no further `Fail` rule is enabled, so the next
`find_failure_transition` returns `None` and the failure is surfaced as a metric
+ Observe event rather than looping. Well-formed specs therefore terminate in at
most one compensation step; there is no explicit depth counter.

## Consequences

- Any integration failure with no declared recovery now surfaces: inline as
  `success: false`, background as a compensating `Fail` transition or, failing
  that, a surfaced metric + Observe event.
- `on_failure` (including `on_failure = "Fail"`) remains the author's explicit,
  preferred recovery hook and is unchanged.
- DST: the compensation dispatch is deterministic; the *background trigger* stays
  on the existing `// determinism-ok` spawn boundary. The hot write path is not
  serialized.
