# ADR-0175: Bounded Validation Feedback Without Coverage Loss

- Status: Proposed
- Date: 2026-08-22
- Deciders: Temper core maintainers
- Related:
  - ADR-0016: Verification Cascade Hardening
  - ADR-0017: Platform Deterministic Simulation Testing
  - ADR-0174: Faithful Deterministic Actor Recovery
  - `.github/workflows/ci.yml`
  - `.claude/hooks/pre-push.sh`

## Context

Temper's required validation coverage is correct but its feedback path is not
bounded. Recent successful full-gate runs show that the random platform DST
suite can take more than 40 minutes, the ordinary workspace suite about 20
minutes, and bench compilation more than 30 minutes. The existing CI workflow
partitions four DST groups, but each group uses a suite-specific build cache,
the ordinary workspace suite remains serial, and local validation offers only
an all-or-nothing pre-push command. The workflow also does not publish a stable,
machine-readable comparison between observed durations and feedback budgets.

Reducing coverage, sampling required merge tests, or replacing the final gate
with change detection would make feedback faster by weakening acceptance. This
decision instead separates early feedback from final acceptance, partitions
independent work across a bounded number of workers, and makes the complete
coverage contract auditable.

## Decision

### One Canonical Validation-Lane Contract

The repository will define validation lanes in a versioned manifest consumed by
the local validation runner and checked against CI. Each lane records its scope,
command, feedback budget, and whether it belongs to pull-request, main, or local
validation. The manifest is the authoritative inventory for formatting,
checking, linting, integrity ratchets, Rust tests, backend parity,
observe-feature tests, DST, spec verification, bench compilation, and
instrumentation hygiene. If a check must remain inline in workflow YAML, the
manifest names that check and the contract validator proves the workflow still
contains it.

CI workflow commands must resolve through the same local runner used by
developers. A contract check rejects a missing lane, duplicate lane, unknown
lane, or workflow reference to a lane that is not in the manifest.

**Why this approach**: A shared executable contract prevents local guidance,
CI matrices, and documented coverage from drifting independently.

### Layered Feedback With Identical Final Acceptance

Validation has four named modes:

1. `fast` runs formatting, workspace checking, linting, integrity ratchets, then
   invokes the same conservative classifier and selected package commands as
   `affected` for early local feedback.
2. `affected` selects conservative package tests from the merge-base diff. Any
   dependency, workspace, workflow, build-script, or unclassified change widens
   selection to the complete ordinary-test lanes.
3. `backend-parity` runs the real PostgreSQL and Redis durability contract and
   is never inferred from an in-memory backend result.
4. `full` runs every required lane and mirrors the merge contract locally. The
   pre-push hook retains its existing complete workspace test suite, expressed
   as a canonical lane; neither fast nor affected results can replace it.

Change awareness is advisory acceleration only. It cannot skip `full` on push,
on protected branches, or in the merge gate.

**Why this approach**: Developers get a fast, relevant signal without making a
heuristic part of the correctness boundary.

### Bounded Sharding and Unified Compilation Inputs

Independent ordinary workspace and deterministic-simulation suites will run as
bounded CI matrices. Shards partition named Cargo packages or test binaries;
they do not filter individual test cases by substring. Feature flags are
canonical per lane so the same target is not repeatedly rebuilt under
accidentally different feature sets within a job.

The number of concurrent shards is explicitly capped. Random DST runs its
complete seed and operation budgets in the pull-request merge gate,
protected-branch pushes, scheduled runs, and local `full` mode. Smoke mode is an
early-feedback option only and never contributes required merge evidence.

**Why this approach**: Named Cargo targets are stable, reviewable units of
coverage, and a concurrency cap bounds hosted-runner consumption.

### Cache Inputs Describe Compatibility, Not Job Names

Rust caches share a namespace when their artifacts are compatible. Cache keys
include the operating system, pinned toolchain, Cargo lockfile, and a hash of
build-affecting manifests/configuration. Restore keys may reuse compatible
registry and target artifacts across validation lanes, while exact keys remain
specific enough to prevent stale configuration from masquerading as a hit.

CI records Cargo compiler timing data for the expensive compile lanes and
uploads it with validation timing results. Cache save failures do not mask test
results, and cache restoration is never treated as evidence that a check ran.

**Why this approach**: Job-name-specific keys discard compatible work; keys
based only on `Cargo.lock` can reuse artifacts across incompatible build inputs.

### Feedback Budgets Report Regressions Without Truncating Proof

Every lane has a wall-time budget in the manifest. The runner emits a stable
JSON result for each lane containing the command identity, duration, budget,
outcome, and budget status. CI uploads the combined report and writes a job
summary. Pull-request budget regressions are visible but non-blocking until a
maintainer deliberately ratchets the budget; command failures always block.

Workflow job timeouts remain larger safety bounds and must not be used as the
feedback budget. A timeout is a failed proof, never a successful shortcut.

**Why this approach**: Measurements can be introduced immediately and ratcheted
from repeated evidence without creating flaky gates or hiding slow successes.

## Rollout Plan

1. Add the canonical lane manifest, runner, contract tests, local guidance, and
   machine-readable timing reports.
2. Rewire ordinary and DST CI jobs to bounded matrices using the runner, align
   compatible cache keys, and upload timing/compiler artifacts.
3. Keep all existing final-gate commands represented, run the full gate on the
   branch repeatedly, and compare both command coverage and wall time with the
   recorded baseline runs before merge.

## Readiness Gates

- Every pre-existing required validation command maps to a canonical lane.
- Local `full` validation and CI matrices consume the same lane definitions.
- A contract test detects omissions and duplicate shard coverage.
- Backend parity still uses real PostgreSQL and Redis services.
- Protected-branch and scheduled runs retain complete random DST coverage.
- Timing artifacts include observed duration, configured budget, and outcome.
- Repeated full-gate runs produce equivalent pass/fail coverage and improve
  wall-clock feedback relative to the recorded baseline.

## Consequences

### Positive

- Independent failures arrive earlier because long suites no longer serialize
  unrelated checks.
- Developers can run conservative affected-package validation before the
  unchanged full pre-push contract.
- Compatible compilation artifacts can be reused across CI lanes.
- Feedback-time regressions become queryable artifacts rather than anecdotes.
- The manifest makes coverage equivalence mechanically reviewable.

### Negative

- CI uses more concurrent workers during the bounded matrix window.
- The lane manifest must be updated when packages or integration-test targets
  are added or renamed.
- Target-directory caches remain large and require careful save discipline.

### Risks

- A bad change classifier could omit an affected package. The classifier is
  conservative and never replaces full validation.
- A shard definition could accidentally duplicate or omit a target. Contract
  validation rejects both conditions against the declared coverage inventory.
- Cache incompatibility could cause confusing build failures. Configuration and
  toolchain hashes are part of the key, and a clean build remains authoritative.
- More workers can increase compute consumption. Matrix concurrency is capped,
  and expensive bench compilation remains outside pull-request events.

### DST Compliance

This change does not alter simulation-visible Rust behavior. DST commands retain
their existing seed/mode semantics and named integration-test targets; only
their orchestration, caching, measurement, and worker placement change.

## Non-Goals

- Reducing deterministic-simulation seeds or fault coverage.
- Treating affected-package selection as merge evidence.
- Replacing PostgreSQL or Redis parity with simulated storage.
- Changing runtime, storage, security, or Cedar behavior.
- Making performance-budget overruns blocking without repeated baseline data.

## Alternatives Considered

1. **Install a third-party test orchestrator** — This could provide richer
   partitioning, but adds supply-chain and installation overhead before the
   repository has measured whether stable Cargo-target shards are insufficient.
2. **Run fewer tests on pull requests** — Rejected because it moves correctness
   discovery after review and weakens the existing merge contract.
3. **Cache one global `target/` archive** — Rejected because unrelated feature
   and configuration inputs make the archive both oversized and unreliable.
4. **Make initial budgets blocking** — Rejected because cold hosted runners and
   cache eviction would create failures unrelated to correctness.

## Rollback Policy

Revert the workflow and runner changes together, restoring direct Cargo commands
from the parent commit. Timing artifacts and this ADR may remain as historical
evidence; no runtime data or deployed state requires migration.
