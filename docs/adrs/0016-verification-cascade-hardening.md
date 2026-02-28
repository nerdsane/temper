# ADR-0016: Verification Cascade Hardening

- Status: Accepted
- Date: 2026-02-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0014: Governance Gap Closure
  - `.vision/VERIFICATION.md` (5-level cascade definition)
  - `crates/temper-verify/src/model/builder.rs` (invariant classifier)
  - `crates/temper-verify/src/cascade.rs` (cascade orchestrator)
  - `crates/temper-server/src/entity_actor/sim_handler.rs` (assertion parser)

## Context

A review of the 4-level verification cascade revealed four weaknesses:

1. **Invariant classification uses crude string matching.** `builder.rs:resolve_invariants` uses `str::contains("> 0")` and `str::split('>')` to classify assertion expressions. Unrecognized expressions silently fall through to `Implication` with empty `required_states`, which gets a free pass (no real verification). Meanwhile, a proper assertion parser already exists in `temper-server/src/entity_actor/sim_handler.rs:97-158`, parsing `never()`, `ordering()`, and generalized counter comparisons (`>=`, `<=`, `==`, `<`).

2. **Cascade runs all levels unconditionally.** The vision doc specifies sequential levels (L1 runs only after L0 passes), but the code runs all levels regardless of failures. This wastes compute on later levels when an early level has already found issues.

3. **L2b Actor Simulation is never wired up.** The `ActorSimRunner` callback type and `run_actor_simulation()` method exist, but no caller provides a runner. The deploy pipeline in `temper-platform` has all the dependencies needed to build one.

4. **L3 uses lightweight mode only.** The proptest shrinking mode (`run_prop_tests_with_shrinking_from_ioa`) is implemented and tested but the cascade always calls the non-shrinking `run_prop_tests_from_ioa`. Shrinking provides minimal counterexamples, which are far more useful for debugging.

## Decision

### Sub-Decision 1: Unified Assertion Parser in temper-spec

Extract the assertion parser from `temper-server/sim_handler.rs` into `temper-spec/src/automaton/assert_parser.rs` as a shared module. Both `temper-verify` (model builder) and `temper-server` (sim handler) import from the same parser. Define `ParsedAssert` and `AssertCompareOp` enums with `parse_assert_expr()` as the single parsing function.

**Why this approach**: Eliminates code duplication and ensures the verify-time classifier recognizes the same assertion patterns as the runtime. `temper-spec` has no internal dependencies, making it the natural home.

### Sub-Decision 2: Explicit Unverifiable Variant

Add `Unverifiable { expression: String }` to `InvariantKind`. Assertions that the parser doesn't recognize (or that map to patterns not encodable at model level, like `OrderingConstraint`) get classified as `Unverifiable` instead of silently passing as `Implication`. Warnings are collected in `CascadeResult.warnings` so developers see what isn't being checked.

**Why this approach**: Silent passes are dangerous. Making unverifiable assertions explicit surfaces coverage gaps without breaking existing specs.

### Sub-Decision 3: Cascade Short-Circuit

Add `fail_fast: bool` to `VerificationCascade`. When enabled, the cascade returns early after the first failing level. `CascadeResult.levels` will contain only the levels that ran.

**Why this approach**: Saves compute and gives faster feedback. The default remains `false` for backward compatibility; the deploy pipeline can opt in.

### Sub-Decision 4: L3 Shrinking Mode

Switch the cascade's L3 from `run_prop_tests_from_ioa` to `run_prop_tests_with_shrinking_from_ioa`. Change `prop_test_cases` from `u64` to `u32` to match proptest's `Config::cases` type.

**Why this approach**: Shrinking is already implemented and tested. Minimal counterexamples are strictly more useful than random failure traces.

## Rollout Plan

1. **Phase 1 (This PR)**: Unified parser, extended InvariantKind, rewritten classifier, new variants wired through all 4 levels, warnings in CascadeResult, sim_handler uses shared parser, test expectations updated.
2. **Phase 2 (This PR)**: Cascade short-circuit with fail_fast builder.
3. **Phase 3 (Follow-up)**: L2b actor simulation wiring in deploy pipeline.
4. **Phase 4 (This PR)**: L3 shrinking mode.

## Consequences

### Positive
- Unverifiable invariants now surface warnings instead of silently passing.
- `NeverState` and `CounterCompare` invariants get real verification across all 4 levels.
- Single assertion parser shared between verify-time and runtime.
- Faster cascade feedback when fail_fast is enabled.
- Minimal counterexamples from L3 shrinking aid debugging.

### Negative
- `CounterCompare` and `NeverState` variants add match arms to 4 files (stateright_impl, smt, simulation, proptest_gen). This is the cost of the multi-level architecture.
- Existing specs that relied on silent Implication fallback will now see warnings (intentional).

### Risks
- Changing `prop_test_cases` from `u64` to `u32` could theoretically break callers passing values > u32::MAX, but all current callers use small values (50-1000).

### DST Compliance
- Phase 1F modifies `temper-server/src/entity_actor/sim_handler.rs`. The change removes a private function and replaces it with an import — no behavioral change to the SimActorHandler impl.
- No new non-determinism introduced.

## Non-Goals

- `OrderingConstraint` verification at model level (requires path history tracking).
- L2b actor simulation wiring (deferred to follow-up).
- Changing the cascade level ordering or adding new levels.

## Alternatives Considered

1. **Keep assertion parser in temper-server** — Rejected because temper-verify cannot depend on temper-server (reverse dependency direction).
2. **Add OrderingConstraint to InvariantKind** — Deferred. Requires tracking state visit history in the model, which is a larger change. The runtime sim_handler already handles it.
3. **Always fail-fast** — Rejected. Running all levels provides a complete picture useful for spec development.
