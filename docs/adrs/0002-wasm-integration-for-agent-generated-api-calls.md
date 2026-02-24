# ADR-0002: WASM Sandboxed Integration Runtime for Agent-Generated API Calls

- Status: Proposed
- Date: 2026-02-23
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-jit/src/table/types.rs` (Effect::Custom already exists)
  - `crates/temper-spec/src/automaton/types.rs` (Integration struct, webhook-only)
  - `crates/temper-server/src/webhooks.rs` (current fire-and-forget webhook dispatch)

## Context

Temper's vision is evolving toward an agent operating system where agents write specs and execute work through Temper. Agents need to call external APIs (Stripe, Gmail, Twilio, etc.) as part of entity lifecycle transitions.

Current integration support is webhook-only (fire-and-forget HTTP POST with fixed templates). This is insufficient for:
- Response parsing and conditional logic (e.g., extract charge ID from Stripe)
- Multi-step API flows (e.g., OAuth token refresh before API call)
- Agent-generated, sandboxed, auditable integration logic attached to entities

## Decision

Use WebAssembly (Wasmtime) as the sandboxed runtime for agent-generated integration handlers.

### Sub-Decisions

1. **Async post-transition execution model** — WASM runs after the transition succeeds, as a fire-and-forget async task. The result feeds back as a new input action (e.g., `ChargeSucceeded` / `ChargeFailed`). Entity goes to an explicit pending state.

   Why: Matches IOA theory (output action → environment → input action). Entity never blocks on external I/O. Pending state is auditable and queryable. DST mocks the callback delivery, not the HTTP call. Model checker explores both success/failure callback paths.

2. **Minimal host ABI** — 3 host functions: `host_http_call`, `host_get_secret`, `host_log`. The platform provides generic capabilities; WASM modules provide API-specific logic.

3. **Wasmtime runtime** — Reference WASM implementation with fuel metering (TigerStyle budgets), memory limits, epoch interruption. Component Model for typed WIT interfaces.

4. **Separate storage** — `wasm_modules` table, not blob column on specs. Enables module reuse across entities, independent versioning, size separation.

5. **DST by callback injection** — Simulation does NOT execute WASM. Callbacks are injected directly by the SimScheduler. The spec defines both success and failure paths through explicit states + input actions. The model checker explores both.

### Spec Pattern

```toml
[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"
effect = "trigger charge_payment"

[[action]]
name = "ChargeSucceeded"
kind = "input"
from = ["ChargePending"]
to = "Confirmed"

[[action]]
name = "ChargeFailed"
kind = "input"
from = ["ChargePending"]
to = "PaymentFailed"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
module = "stripe_charge"
on_success = "ChargeSucceeded"
on_failure = "ChargeFailed"
```

## Consequences

### Positive
- Agents can call any API without platform changes — one generic HTTP host function covers all APIs
- All integration work is sandboxed, auditable, and verifiable
- DST-compatible by construction — entity never blocks on external I/O
- Pending states are explicit in the spec, queryable, and observable
- Module reuse across entities and tenants

### Negative
- Adds wasmtime dependency (large compile time increase)
- Requires output→pending→input pattern in every integration spec
- WASM module compilation adds deploy latency
- WIT/Component Model is still evolving

### Risks
- Wasmtime version churn may require periodic migration
- Large WASM modules could impact cold-start latency
- Agent-generated WASM code quality varies

## Alternatives Considered

1. **Declarative HTTP templates** — Simpler but cannot handle response parsing, conditional logic, or multi-step APIs.
2. **Native Rust plugins (dylib)** — No sandboxing, not portable, harder to deploy.
3. **Embedded scripting (Lua/JS)** — Less sandboxed, no standard component model, harder to bound resources.
4. **Synchronous WASM execution (blocking actor)** — Violates actor non-blocking principle, harder for DST.
