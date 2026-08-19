# ADR-0169: Automaton is the sole authored behavior IR

- Status: Accepted
- Date: 2026-08-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0016: Verification cascade hardening
  - [ARN-382](https://linear.app/arni-build/issue/ARN-382): TLA+/StateMachine leftover cleanup
  - [ARN-383](https://linear.app/arni-build/issue/ARN-383): CSDL generation follow-up (out of scope here)
  - `crates/temper-spec/src/automaton/` (authored IR)
  - `crates/temper-verify/src/model/` (`TemperModel` view)
  - `crates/temper-jit/src/table/` (`TransitionTable` view)

## Context

Behavior is authored as I/O Automaton TOML and parsed to [`Automaton`]. Two leftover paths still treated TLA+ / `StateMachine` as a peer IR:

- `temper-spec` extracted TLA+ into `StateMachine` and converted `Automaton` through `to_state_machine`.
- `temper verify` ran the L0–L3 cascade on IOA, then printed a second report from `build_spec_model(csdl, tla_sources)` with TLA transition counts and the stale line “Full model checking (Stateright) is not yet integrated.”
- `VerificationCascade::run` stored the TOML string and re-parsed it at L0 (SMT), L2 (simulation), and L3 (proptest) via `build_model_from_ioa` / `run_*_from_ioa`.

`TemperModel` and `TransitionTable` already build from `Automaton`. The TLA extractor and `StateMachine` were not on the production evaluate path.

## Decision

### Sub-Decision 1: `Automaton` is the only authored behavior IR

I/O Automaton TOML is parsed once to `Automaton`. That value is the behavior IR for lint, the verification cascade, and the runtime table.

**Why this approach**: One parse, one type. Verification and evaluate cannot diverge through a second extractor.

### Sub-Decision 2: `TemperModel` and `TransitionTable` stay as views

Do not delete `TemperModel` or `TransitionTable`. They remain derived views:

- `TemperModel` — Stateright / SMT / simulation / proptest view (`build_model_from_automaton`)
- `TransitionTable` — runtime evaluate view (`TransitionTable::from_automaton`)

Convenience wrappers that parse TOML then delegate (`from_ioa`, `build_model_from_ioa`, `from_ioa_source`, `run_*_from_ioa`) may remain for tests and CLI stdin. `VerificationCascade::run` must use one `Automaton` (or one `TemperModel` built from it).

**Why this approach**: Callers that already hold TOML keep a one-liner. The cascade and registry that already parsed do not parse again.

### Sub-Decision 3: Remove TLA+ / `StateMachine`

Deleted:

- `crates/temper-spec/src/tlaplus/` (extractor + `StateMachine`)
- `to_state_machine`
- `test-fixtures/specs/order.tla`

CSDL `TlaSpec` annotations remain as ignored XML until ARN-383. They do not require a `.tla` file.

`SpecModel` links CSDL to `Automaton` only. TLA-only APIs (`SpecSource::Tla`, `build_spec_model` taking TLA maps) are gone.

`temper verify` prints the L0–L3 cascade it already ran. It does not print TLA transition counts or claim Stateright is unintegrated.

## Rollout Plan

1. **Immediate** — This change: ADR, delete leftover TLA IR, parse once in the cascade, update CLI report and docs that claimed TLA was live.
2. **Follow-up (ARN-383)** — Replace TOML+CSDL authoring with P input. Do not generate or delete CSDL here.

## Consequences

### Positive

- One behavior IR; cascade and table share the same parsed `Automaton`.
- `temper verify` report matches the cascade that actually ran.
- TLA stdin to `verify-ioa` fails as a parse error instead of panicking inside `expect`.

### Negative

- Historical TLA fixtures and extractor tests are gone. Recovery is git history plus this ADR.

### Risks

- Any overlooked `StateMachine` caller fails to compile. Mitigation: workspace grep for `extract_state_machine`, `to_state_machine`, `pub struct StateMachine`, `order.tla`.
- CSDL still required to serve. Left as-is (ARN-383).

### DST Compliance

No new simulation-visible non-determinism. `temper-spec` may use `HashMap` for `SpecModel` linking. JIT/server continue to build tables from `Automaton` with `BTreeMap` indexes.

## Non-Goals

- CSDL generation or deletion (ARN-383).
- Deleting `TemperModel`.
- L2b / actor-sim wiring (ADR-0016 still open).
- Replacing TOML+CSDL with P input.

## Alternatives Considered

1. **Keep `StateMachine` as a shared IR** — Rejected. Both views already build from `Automaton`; a third type only existed to carry TLA extracts.
2. **Delete `TemperModel` and drive Stateright from `Automaton`** — Rejected. Out of scope; `TemperModel` is the verification view.

## Rollback Policy

Revert this change. TLA sources remain in git history (`order.tla` and `crates/temper-spec/src/tlaplus/`).
