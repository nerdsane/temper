# ADR-0161: Native adapter selection must stay within the spec's declared set

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/state/dispatch/adapter.rs` (adapter dispatch)
  - `crates/temper-server/src/adapters/` (native adapter implementations)
  - ARN-228 (security finding)

> This is Fable's competing entry for ARN-228; the two arena entries are compared
> head-to-head by the judge.

## Context

Integrations of `type = "adapter"` run in **native platform Rust, outside the WASM
sandbox** (`crates/temper-server/src/adapters/`). The built-in adapters spawn host
CLI processes (`claude_code`, `codex` → `tokio::process::Command`), open outbound
WebSocket/HTTP connections (`openclaw`, `http`), and every adapter invocation is
handed the tenant's credential context.

Which adapter runs is resolved at dispatch time (`dispatch_single_adapter_integration`)
from a **mutable entity state field**:

```rust
let adapter_type = entity_state.fields.get("adapter_type")…      // attacker-writable
    .or_else(|| integration.config.get("adapter").cloned())      // spec-declared
    .or_else(|| integration.config.get("adapter_type").cloned())
    .ok_or_else(|| … "missing required config key 'adapter'")?;
```

The entity field wins over the spec's declaration, and it may name **any** adapter
registered in the process, with no bound from the spec. So a spec that declares a
benign `adapter = "http"` integration can be pivoted at runtime, by any principal
able to write the `adapter_type` field through a normal production action, into
running the `codex` / `claude_code` host-process adapter — arbitrary host process
execution the spec never authorized. The declared adapter is supposed to be the
authorization boundary; the mutable field escalates straight past it. This is a
sandbox-escape / privilege-escalation boundary, not a functional detail.

Dynamic adapter selection is itself a **legitimate, tested capability**
(`adapter_integration_uses_entity_adapter_type_over_static_config`): some specs
switch adapters based on entity state. The defect is that the switch is unbounded,
not that it exists.

## Decision

**An entity-provided `adapter_type` may only select an adapter the integration's
spec explicitly declared.** Dispatch computes the *permitted set* from the
integration config — `adapter`, `adapter_type`, and a new `allowed_adapters`
(comma/space-separated list) — and the effective adapter must be a member:

- No entity override → the declared primary adapter (`adapter` / `adapter_type`).
- Entity override present → allowed **iff** it is in the permitted set; otherwise
  the integration fails closed with an explicit error (routed through the existing
  `on_failure` compensation, ADR-0152 — never a silent drop, never a host spawn).

A spec that wants runtime adapter switching declares the alternatives up front:

```toml
[[integration]]
name = "adapter_call"
type = "adapter"
adapter = "http"
allowed_adapters = "http, claude_code"   # explicit, spec-authored authorization
```

The spec author — whose changes pass the verification cascade and the Cedar
spec-change gate — remains the sole authority over which unsandboxed adapters an
entity may reach.

## Consequences

### Positive
- The mutable entity field can no longer escalate a benign integration into host
  process execution or an unintended outbound adapter. The authorization boundary
  is the spec, as intended.
- Dynamic selection still works, now explicitly bounded and auditable in the spec.

### Migration / capability preservation
- Specs that declared a single `adapter` and never overrode it are unaffected.
- A spec that relied on entity-field override now declares `allowed_adapters`. This
  is a one-line, additive declaration — the capability is preserved, made explicit,
  and least-privilege. No working capability is removed; an *unbounded* one is
  bounded. (In-repo, the only consumer is the dispatch test, migrated here.)

### DST Compliance
- `temper-server` is simulation-visible. The change is pure, deterministic
  validation (`BTreeSet` membership over `BTreeMap` config); no wall clock, no
  threads, no `HashMap`, no ambient I/O. A deterministic unit test covers the
  permitted/rejected/default branches, and an integration test proves the live
  before/after at the dispatch layer with a recording canary adapter (no real
  process spawn).

## Non-Goals / Follow-ups

- **Per-integration secret scoping.** Every adapter invocation still receives the
  full tenant secret snapshot (`get_tenant_secrets`) rather than only the secrets
  its integration references. Narrowing that to a declared, least-privilege set is
  a related hardening tracked as a follow-up; it is independent of the selection
  boundary closed here and touches the secrets-template surface.
- **SSRF hardening of the `http`/`openclaw` adapters.** Their outbound clients are
  unbounded/redirect-following (cf. ARN-210's registry-fetch hardening); applying
  the same host guard is a separate follow-up.

## Alternatives Considered

1. **Remove entity-field selection entirely (spec `adapter` only).** Rejected: it
   drops a legitimate, tested capability. Bounding the override preserves it while
   closing the escalation.
2. **Allowlist only host-process adapters.** Rejected: an allowlist keyed on "which
   adapters are dangerous" is fragile and misses outbound-data adapters; bounding
   selection to the spec's own declaration is exhaustive and self-maintaining.
