# ADR-0050: Mandatory Liveness Coverage for Non-Terminal States

- Status: Proposed
- Date: 2026-04-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0017: Platform deterministic simulation testing (liveness verification exists in DST but not at spec-load)
  - ADR-0049: State-entry timeouts (primitive this ADR mandates)
  - `crates/temper-spec/src/automaton/metadata.rs:131-136` (existing terminal-state detection)
  - `crates/temper-spec/src/automaton/validate.rs` (new)

## Context

The 2026-04-17 Katagami incident traced to Session's `Provisioning` state being effectively a trap state — it had no `TimeoutFail` exit in its `from` list, so the heartbeat watchdog could not sweep stuck sessions. No tool caught this before deploy.

Temper already verifies liveness properties in DST (`LivenessViolation` type, `check_liveness_post_simulation`), but those checks are simulation-only — they run when a scenario is explicitly written. Nothing runs at spec-load time to reject a spec that ships a state with no path out.

ADR-0049 introduces `[[state_timeout]]` as a declarative primitive. On its own, that is not enough: authors must *remember* to use it on every non-terminal state. This ADR closes that gap by making coverage mandatory at spec-compile time.

## Decision

Reject at spec-load any spec where a non-terminal state lacks either a `[[state_timeout]]` declaration or an entry in the entity's top-level `allow_indefinite_states = [...]` list.

### Sub-Decision 1: The invariant

For every entity type E and every state S ∈ states(E):

```
S is terminal(E)                                       // no outgoing transitions
  OR ∃ [[state_timeout]] with state == S
  OR S ∈ allow_indefinite_states
```

"Terminal" uses the existing computation at `metadata.rs:131-136`:

```rust
let terminal_states: Vec<String> = states
    .iter()
    .filter(|s| !from_states.contains(*s))
    .cloned()
    .collect();
```

**Why this shape**: no new concept of "terminal" — reuse the one the metadata pipeline already builds for Cedar policy evaluation.

### Sub-Decision 2: `allow_indefinite_states` escape hatch

Some states are indefinite *by design* — they wait for an external signal that has no timeout. Examples:
- `Session.WaitingForApproval` — waits for a human Cedar decision.
- `HeartbeatMonitor.Idle` — waits for the scheduled-scan cycle.

These declare themselves explicitly:

```toml
allow_indefinite_states = ["WaitingForApproval"]
# justification: Cedar approval is human-gated. A timeout would conflict with
# governance semantics (a denied decision is a decision; a timed-out one is a policy gap).
```

The `# justification:` comment is a lint-enforced requirement (not a compiler one — comments don't survive round-tripping cleanly). Spec review checks it.

**Why an allowlist and not a per-state flag**: visibility. One place lists every state this entity explicitly refuses to time out. Reviewers read it in full. A per-state `indefinite = true` flag scatters the decision across the spec.

### Sub-Decision 3: Hard failure at spec load

`validate_liveness_coverage(spec) -> Result<(), SpecError>` runs during `load_specs`. Failure is `SpecError::UncoveredNonTerminalState { entity, state }` with the offending state surfaced in the error message. Server boot fails; the process exits with a clear diagnostic.

Feature flag `TEMPER_LIVENESS_ENFORCE = true` by default. Setting `false` demotes failures to warnings emitted to logs and to `temper_spec_liveness_violations_total`. The flag exists only to ease the very first upgrade; it is removed after ADR-0036 migrates Session and ADR-C2 migrates the remaining specs.

**Why hard-fail**: warnings get filtered. The incident's whole point is that nobody noticed the trap state until it bit production. Compilation failure forces the conversation to happen at review time.

### Sub-Decision 4: CI gate via `verify_specs`

The existing `verify_specs` binary (used as CI for spec changes) gains a call to `validate_liveness_coverage`. PRs that introduce or modify an `.ioa.toml` with an uncovered state cannot merge.

**Why**: production boot-fail is the backstop. CI is where the author sees the error first.

## Rollout Plan

1. **Phase 0** — Implement `validate_liveness_coverage`. `TEMPER_LIVENESS_ENFORCE=false`. Spec load logs warnings for every violation.
2. **Phase 1** — Fleet survey: run the validator against every installed spec. Fix violations in priority order (ADR-0036 for Session first; C2 task for the rest).
3. **Phase 2** — Flip `TEMPER_LIVENESS_ENFORCE=true` on dev, then staging.
4. **Phase 3** — Production. Any new spec violation blocks deploy.
5. **Phase 4** — Remove the flag. Enforcement is unconditional.

## Readiness Gates

- Zero `temper_spec_liveness_violations_total` in staging for 7 days before prod flip.
- Every installed spec lists its indefinite states with a reviewed justification.
- `verify_specs` CI gate green on every PR for 14 days.

## Consequences

### Positive
- Trap states are structurally impossible after rollout.
- Every reviewer of a new spec sees the liveness commitment in one place (`[[state_timeout]]` blocks + `allow_indefinite_states`).
- Production cannot boot with a regression in this class.

### Negative
- Existing specs must be migrated (ADR-0036 and C2). One-time cost but nontrivial.
- A few legitimate indefinite states (e.g., long-lived queue processors) now require explicit justification text. Hygiene, not friction.

### Risks
- **False positives on ambiguous states.** A state used for both "brief transient hop" and "long wait" might look indefinite to the validator. Mitigation: split the state into two. If reviewers can't distinguish the two uses, the spec itself is the problem.
- **Flag-off drift.** Teams living on `TEMPER_LIVENESS_ENFORCE=false` indefinitely. Mitigation: calendar commit to remove the flag by end of rollout phase, not optional.

### DST Compliance
- Static validation runs at spec-load, before any simulation. Zero DST impact.

## Non-Goals

- Deadlock detection beyond per-state timeouts (cycle detection, liveness under fairness). DST continues to handle those at scenario time.
- Quantitative liveness ("95% of sessions reach Completed within 30min") — this is an SLO for Datadog, not a spec property.

## Alternatives Considered

1. **Warning only** — Rejected. Incident proves warnings get missed.
2. **Per-state opt-in flag** — Rejected. Scatters the allowlist across the spec; reviewers need one place to see every exception.
3. **Runtime-only detection (monitor stuck entities, page on detection)** — Rejected as sole mechanism. Reactive by design; relies on production being observed correctly; doesn't prevent the class of bug.

## Rollback Policy

Set `TEMPER_LIVENESS_ENFORCE=false` to restore warning-only behavior. Non-destructive; zero persistent-state impact. Existing specs continue to load.
