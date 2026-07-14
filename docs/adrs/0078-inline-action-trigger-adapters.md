# ADR-0078: Inline Action Trigger Adapters

- Status: Accepted
- Date: 2026-05-02
- Deciders: Temper core maintainers
- Partially superseded by: ADR-0171 (webhook expansion statement only)
- Related:
  - ADR-0046: Unified Action Triggers
  - `crates/temper-spec/src/automaton/parser.rs`
  - `crates/temper-server/src/state/dispatch/adapter.rs`

## Context

ADR-0046 made `[[action.triggers]]` the canonical way to declare action-local outgoing work. At the time of this decision the parser synthesized runtime integrations for `kind = "wasm"` and `kind = "webhook"`, while native adapter execution still depended on legacy `[[integration]] type = "adapter"` declarations. ADR-0171 later rejected webhook declarations because no durable runtime consumed them; the WASM comparison remains valid here.

That gap leaves test and app specs unable to override an inline WASM trigger with a deterministic native adapter without falling back to older integration syntax. In the GEPA autonomous-loop test, the stale override failed to replace the inline proposer trigger, so CI attempted to execute the unregistered `gepa-proposer-agent` WASM module and transitioned the run to `Failed` before `RecordMutation`.

## Decision

Support `kind = "adapter"` in `[[action.triggers]]`.

Adapter triggers synthesize the same `Integration` metadata shape as legacy adapter integrations:

```toml
[[action.triggers]]
name = "propose_mutation"
kind = "adapter"
adapter = "claude_code"
on_success = "RecordMutation"
on_failure = "Fail"

[action.triggers.config]
command = "/path/to/mock"
```

The parser adds the synthesized trigger effect to the source action, validates that `adapter` or `adapter_type` is present, validates callback actions, and flattens adapter-specific fields into the integration config consumed by the existing adapter dispatcher.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add parser/type support and focused parser tests. Update the GEPA autonomous-loop test to use inline adapter triggers for deterministic CI overrides.
2. **Phase 1 (Follow-up)** — Migrate any remaining app specs that use legacy adapter integrations when they naturally move to ADR-0046 syntax.

## Consequences

### Positive

- Native adapters become first-class ADR-0046 trigger targets.
- Tests can override LLM/WASM integration points with deterministic native adapters without preserving old syntax.
- Runtime behavior stays centralized in the existing adapter dispatcher.

### Negative

- `ActionTrigger` grows one more kind-specific field.
- Specs can now declare local process execution through inline trigger syntax, so governance and review need to treat `kind = "adapter"` with the same care as legacy adapter integrations.

### DST Compliance

- The spec parser remains deterministic and uses ordered maps already present in `ActionTrigger`.
- Adapter execution remains outside simulation-visible core semantics; this ADR only changes metadata synthesis.

## Non-Goals

- This does not change adapter execution semantics, credential minting, process spawning, or callback dispatch.
- This does not migrate existing production specs from WASM proposer agents to native adapters.

## Alternatives Considered

1. **Use legacy `[[integration]] type = "adapter"` only in tests** — Rejected because it preserves the ADR-0046 gap and makes inline trigger overrides fragile.
2. **Keep GEPA proposer as WASM in CI** — Rejected because the test intentionally avoids live LLM keys and external verification services.

## Rollback Policy

Remove `TriggerKind::Adapter`, parser synthesis/validation, and the parser tests. Specs can fall back to legacy adapter integrations if necessary.
