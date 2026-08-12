# ADR-0160: Native adapter sandbox boundary (ARN-228)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ARN-228: Native adapter effects bypass the sandbox
  - ARN-208: HttpEndpoint WASM host trust boundary (related trust edge)
  - ADR-0152: Integration failure is never silent
  - ADR-0033: Platform-assigned agent identity (credential minting no longer used for in-kernel adapters)
  - `crates/temper-server/src/adapters/`
  - `crates/temper-server/src/state/dispatch/adapter.rs`

## Context

The kernel registered Claude Code, Codex, OpenClaw, and HTTP adapters as a
second privileged execution system outside the governed WASM host. Dispatch
preferred a mutable entity `adapter_type` field over the declared integration,
cloned the full tenant secret map into `AdapterContext`, and (for process
adapters) spawned host processes that inherited the server environment and
received a minted platform credential.

A hostile or buggy spec could therefore exfiltrate secrets to an arbitrary
origin, hit private/metadata addresses, or run a host-level coding agent with
full ambient authority. That bypasses the WASM/Cedar trust boundary.

Repository scope: agent/app adapters belong in a capability-scoped TemperPaw
worker, not in the Temper kernel process. The kernel keeps only a generic
integration-intent path with fail-closed admission.

## Decision

### Sub-Decision 1: Ownership

- **Temper kernel**: durable integration *intent* (declared adapter type,
  resolved config with least-privilege secret templates, egress origin policy,
  budgets). No app-specific agent CLIs.
- **TemperPaw (follow-up)**: sandboxed execution of Claude/Codex/OpenClaw and
  other host-level agent tools under typed grants.

### Sub-Decision 2: Remove in-kernel agent process adapters

`claude_code`, `codex`, and `openclaw` implementations are **deleted** from
`temper-server`. `AdapterRegistry::with_builtins` registers only the generic
`http` adapter. Spec strings that still name those adapters fail closed at
dispatch ("adapter not found") and drive ADR-0152 failure compensation.

### Sub-Decision 3: Declared adapter only

Adapter type is taken **only** from the integration declaration
(`config.adapter` / `config.adapter_type`). Entity state fields **must not**
select or escalate the adapter. This closes privilege escalation via mutable
`adapter_type`.

### Sub-Decision 4: Least-privilege secrets

- Resolve `{secret:KEY}` templates into the integration config only.
- Do **not** clone the tenant's full secret map into `AdapterContext`.
- Do **not** mint platform `TEMPER_API_KEY` credentials for in-kernel adapter
  runs (no ambient host-agent authority).

### Sub-Decision 5: HTTP egress gate and budgets

The HTTP adapter:

- Validates URL scheme/host against a private/metadata/loopback policy.
- Disables redirects.
- Applies connect/total timeouts and a response-byte budget.
- Loopback/http is opt-in via `TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK` (tests set
  this). Production defaults to https + non-blocked public hosts.

### Sub-Decision 6: Fail-closed admission

Unknown adapter types, blocked URLs, oversize responses, and timeouts return
adapter failures (never silent success).

## Rollout Plan

1. **Phase 0 (this PR)** — Kernel containment: delete process adapters, declared
   adapter only, secret map removal, HTTP egress + budgets, negative tests.
2. **Phase 1 (TemperPaw)** — Capability-scoped worker executes agent adapters;
   kernel posts durable integration intents / outbox rows with typed grants.
3. **Phase 2** — Drop any transitional compatibility once workers are live.

## Consequences

### Positive

- Specs cannot spawn host CLIs or inherit ambient platform credentials via the
  kernel adapter path.
- Entity data cannot escalate to a privileged adapter.
- Full tenant secret maps are no longer handed to adapters.
- Common SSRF classes (literal private IP, metadata hostnames, redirects) are
  blocked on the remaining HTTP path.

### Negative

- Evolution / GEPA flows that overrode WASM proposers with `claude_code` mock
  scripts must use HTTP mocks or external workers.
- Local webhook development needs `TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK`.

### Risks

- DNS rebinding (public hostname → private A record after validation) is not
  fully closed without connect-time IP pinning; same residual as ADR-0157.
- Crash durability of background adapter intents remains at-least-once via
  existing spawn path until the TemperPaw outbox lands.

### DST Compliance

Adapter I/O remains outside simulation core (`// determinism-ok` on existing
async side-effect paths). No new wall-clock or OS RNG in sim-visible transition
logic.

## Non-Goals

- Implementing the full TemperPaw worker in this PR.
- Cedar policy authoring for every egress origin (policy is structural fail-closed).
- Changing how IOA specs *parse* `adapter = "claude_code"` strings (parse stays;
  runtime registration is gone).

## Alternatives Considered

1. **Keep process adapters with env_clear + budgets only** — Still a host
   breakout surface and violates repo boundary (agent logic in kernel).
2. **Entity allowlist for adapter_type** — Still lets entity data influence
   privilege; rejected.
3. **Disable all adapters** — Breaks legitimate HTTP webhooks; too blunt.

## Rollback Policy

Reverting this PR reintroduces process adapters and the entity `adapter_type`
escalation path. Do not roll back without an equivalent containment.
