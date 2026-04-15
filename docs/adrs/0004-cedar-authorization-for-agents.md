# ADR-0004: Cedar Authorization for Agents

## Status
Accepted

## Context
ADR-003 established agent identity headers, credentials vault, idempotency, and blocking integrations. But authorization was explicitly deferred: agent identity was informational only, WASM host functions were ungated, and all tenant secrets were visible to all modules.

As Temper evolves into a backend for personal assistant agents, we need Cedar policies to control what agents can do at every level.

## Decision
Implement Cedar authorization at three levels with a three-tier policy lifecycle.

### Three Authorization Levels

**Level 1 — Entity Actions**: Wire real request context into Cedar evaluation. Extract `X-Temper-*` headers, enrich with agent identity, and include entity state as resource attributes. The `authorize_with_context()` method on `ServerState` replaces the empty-context `authorize()` call.

**Level 2 — WASM Host Functions**: Gate `http_call` and `get_secret` through a `WasmAuthzGate` trait (dependency inversion — `temper-wasm` does not depend on `temper-authz`). `AuthorizedWasmHost` decorates any `WasmHost` and checks authorization before delegating. Cedar evaluates principals as `Agent::"module_name"` with actions `http_call` and `access_secret`.

**Level 3 — Secret Pre-Filtering**: Defense in depth. Before constructing `ProductionWasmHost`, filter tenant secrets through the authorization gate. Even if the decorator is bypassed, unauthorized secrets aren't in memory.

### Three Policy Tiers

**Tier 1 — Platform Presets**: Shipped with Temper in `platform-presets.cedar`. Uses `forbid` to deny localhost/loopback SSRF, vault master key access, and internal config secrets. Cannot be overridden.

**Tier 2 — Spec-Time Suggested**: When an agent submits a spec with WASM integrations, the system generates suggested Cedar policies (e.g., "stripe_charge needs POST to api.stripe.com"). Developer approves/modifies/rejects before deployment.

**Tier 3 — Evolution-Driven**: Runtime Cedar denials are recorded as `authz_denied` trajectory entries. These surface through the Evolution Engine (O-Record → I-Record pattern) for developer review. Approved denials become persistent policies via `AuthzEngine::reload_policies()`.

### Backward Compatibility
- No Cedar policies loaded → `AuthzEngine::permissive()` → System bypass works as before
- No `X-Temper-*` headers → defaults to System → allowed
- `PermissiveWasmAuthzGate` used when no policies are configured
- Existing fire-and-forget integration path unchanged

### DST Compliance
- All Cedar evaluation is synchronous, CPU-bound
- `BTreeMap` used internally, converted to `HashMap` at Cedar boundary with `// determinism-ok`
- `WasmAuthzGate` is a sync trait
- Domain extraction is pure string parsing

## Consequences
- Agents cannot escalate their own privileges (they propose specs, developers gate policies)
- WASM modules are constrained to authorized domains and secrets
- The evolution loop surfaces authorization gaps for developer resolution
- Platform presets provide non-negotiable security boundaries
