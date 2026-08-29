# WASM integration

## Sub-features
WASM modules fired by transitions via `[[action.triggers]]` (ADR-0046) or legacy `[[integration]]`. Engine in `crates/temper-wasm`; dispatch decision in `crates/temper-server/src/state/dispatch/wasm.rs`.

## How to get to it (user POV)
A transition can fire a WASM module (parse a payload, call an API, compute a result). The module's result can re-enter the machine as a follow-up action - but the module never drives the machine itself.

## Driving it
A module exports `run(i32, i32) -> i32` (`(context_ptr, context_len)` -> result ptr). It receives a `WasmInvocationContext` (tenant, entity, trigger action + params, entity state, integration config) and returns a `WasmInvocationResult` (`callback_action`, `callback_params`, `success`, `error`, `duration_ms`), either by writing to memory or via `host_set_result`. Wire it as a `kind = "wasm"` trigger on an action, then dispatch that action and observe.

Build target: both `wasm32-wasip1` and `wasm32-unknown-unknown` work - WASI is auto-detected from the module's imports. When the app manifest declares a `target`, only that target's release binary is used.

## What proves it
Three observable signals: a row in the `wasm_invocations` table (module, trigger_action, callback_action, success, duration_ms); an `integration_complete` observe event on the entity (`{module, result, callback_action}`); and, if a callback action fired, the source entity's state advanced (read it back over OData).

## Gotchas
- The kernel decides whether to dispatch the module's `callback_action`, not the module. `callback_action = on_success` if the trigger sets it, else the module's returned value; **empty means nothing is dispatched.** A Composite integration returning the default SDK `"callback"` with no `on_success` is zeroed (`composite_result_consumed`, `wasm.rs`) so it does not become an implicit self-dispatch. This is the "inert callback" trap: return `""` for a genuine no-op, not `"callback"`.
- Modules have NO dispatch/transition capability (the host trait exposes HTTP, secrets, streams, spans, and a pure `evaluate_transition` validator - nothing that dispatches). Sequencing belongs to the state machine; a module result re-enters only as `on_success`/`on_failure`. This is the source backing for "a module fired by a transition never dispatches transitions itself."
- Triggers are fire-and-forget post-commit: a failing integration does not roll back the source transition (its `on_failure` action runs instead). Integrations are metadata-only and never affect verification.
- Budgets per invocation: fuel 1e9, memory 64 MB, duration 120 s, response body 1 MB, module size 10 MB.
