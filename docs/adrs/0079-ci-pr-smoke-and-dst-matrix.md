# ADR-0079: CI PR Smoke and DST Matrix

- Status: Proposed
- Date: 2026-05-04
- Deciders: Temper core maintainers
- Related:
  - `.github/workflows/ci.yml`
  - `crates/temper-server/tests/dst_platform_random.rs`

## Context

Temper's pull request CI currently serializes expensive phases behind compile and lint,
and the compile job also builds all benchmarks. That keeps pull request feedback slow
even when test and spec verification work could run independently.

The `dst_platform_random.rs` suite intentionally runs many deterministic seeds. That is
valuable coverage, but it is also one of the slowest PR gates. We still need exhaustive
random coverage before code lands on protected long-lived branches and during unattended
nightly runs.

## Decision

PR CI will cancel superseded runs with a workflow concurrency group based on pull request
number or branch ref. The workflow will also support a nightly schedule and manual
dispatch so full checks can be run without opening a PR.

Compile and lint will stop building benchmark targets. Benchmark compilation moves into
a separate `Bench Build` job that runs only on non-PR events.

Regular workspace tests and spec verification will no longer depend on compile and lint,
allowing independent jobs to run in parallel. The regular test job will skip DST tests
with `cargo test --workspace -- --skip dst_`.

DST and platform tests will move into a dedicated matrix with stable suite names:

- `core`
- `platform-boot`
- `platform-consistency`
- `platform-random`

The `platform-random` suite will read `TEMPER_DST_RANDOM_MODE=smoke|full`. The default
mode is `full`, preserving the current seed and operation budgets. Pull request CI will
set `smoke` only for the `platform-random` matrix row. Pushes to `main` and `staging`,
nightly runs, and manual dispatches will use full mode.

## Rollout Plan

1. **Phase 0 (Immediate)** - Ship the workflow split, benchmark relocation, DST matrix,
   and `dst_platform_random.rs` mode switch.
2. **Phase 1 (Follow-up)** - Update branch protection required checks if exact status
   contexts are configured, adding the new DST matrix contexts while preserving `Tests`.
3. **Phase 2 (Validation)** - Confirm PR runs use smoke mode and post-merge or nightly
   runs use full random coverage.

## Readiness Gates

- Workflow YAML parses successfully.
- `cargo fmt --check` passes.
- Non-DST workspace tests pass with `cargo test --workspace -- --skip dst_`.
- `TEMPER_DST_RANDOM_MODE=smoke cargo test -p temper-server --test dst_platform_random`
  passes.
- The stable DST matrix commands pass locally or in CI.

## Consequences

### Positive

- Pull requests receive faster, parallel feedback.
- Superseded CI runs are canceled instead of consuming runner time.
- Benchmark compilation no longer blocks PR validation.
- Exhaustive random DST coverage remains available on long-lived branches and nightly runs.

### Negative

- PRs no longer run every `dst_platform_random.rs` seed by default.
- Branch protection may need a one-time required-check update for the new DST matrix
  contexts.

### Risks

- A seed-only random regression could escape PR smoke coverage. This is mitigated by
  preserving full deterministic seed coverage on pushes to `main` and `staging`, nightly
  schedule runs, and manual workflow dispatch.

### DST Compliance

- Full mode preserves the existing deterministic seed ranges and operation counts.
- Smoke mode uses deterministic prefixes of the same seed ranges and does not introduce
  wall clock time, random OS entropy, threads, or unordered iteration.

## Non-Goals

- Changing branch protection settings from code.
- Reducing full random DST coverage on `main`, `staging`, nightly, or manual workflow
  runs.
- Changing test semantics outside CI scheduling and random test budgets.

## Alternatives Considered

1. **Keep all DST tests inside `Tests`** - Rejected because DST-heavy suites would
   continue to dominate the primary test job and make required status attribution less
   clear.
2. **Lower full random budgets globally** - Rejected because it would permanently reduce
   coverage instead of limiting the tradeoff to PR feedback loops.
3. **Use ad hoc `--ignore` flags** - Rejected because it would make local and CI behavior
   less explicit than a named environment mode with a clear default.

## Rollback Policy

Revert the workflow job split and remove the `TEMPER_DST_RANDOM_MODE` handling from
`dst_platform_random.rs`. Because full mode is the default, removing the PR smoke
environment variable also restores full random coverage immediately.
