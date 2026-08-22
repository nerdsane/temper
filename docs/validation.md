# Validation lanes

Temper's final acceptance coverage is defined in
`.ci/validation-lanes.json`. The same commands run locally and in CI through
`scripts/validation.py`; CI may place independent lanes on separate bounded
workers, but it does not substitute a different command.

## Local feedback

Run the conservative fast path while iterating:

```bash
python3 scripts/validation.py mode fast --base fork/main
```

`fast` runs formatting, compilation, linting, integrity checks, and tests for
changed workspace packages plus every local reverse dependency. Run only the
affected tests when the mechanical checks have already passed:

```bash
python3 scripts/validation.py affected --base fork/main
```

Changes to workspace manifests, the lockfile, Cargo configuration, workflows,
build scripts, or unclassified source paths widen selection to all workspace
packages. Documentation-only changes may select no Rust tests. This classifier
is an early-feedback aid and is never accepted as the final merge proof.

Backend parity requires live PostgreSQL and Redis endpoints:

```bash
DATABASE_URL=postgres://temper:temper@localhost:5432/temper_test \
REDIS_URL=redis://localhost:6379/0 \
python3 scripts/validation.py mode backend-parity
```

Run every pull-request acceptance lane sequentially with:

```bash
python3 scripts/validation.py mode full
```

This includes complete random DST budgets and is intentionally expensive. The
pre-push hook retains its existing complete `cargo test --workspace` contract
through the `prepush-workspace` lane; it does not use affected-package results.
The merge contract also runs the direct maintained-spec exploration target,
whose test name is intentionally filtered from the ordinary workspace shard.

## Timing and coverage evidence

Every lane writes `target/validation-reports/<lane>.json`, including its exact
argument vector, outcome, duration, budget, and budget status. A budget overrun
is reported but does not turn a passing command into a failure. A command error
or timeout remains a failed proof.

CI uploads lane reports and Cargo's compiler-unit/link timing HTML, then checks
that every required lane produced exactly one report. Baselines from the source
runs are recorded in `.ci/validation-baseline.json`.

The server lane also builds its GEPA WASM fixtures through
`scripts/build-gepa-test-modules.sh`. Those standalone modules share one build
directory to avoid recompiling the SDK, while the script installs each fixture
at the path consumed by the integration tests. This prerequisite is part of the
lane contract rather than workflow-only setup.

Capture a new local compile/link inventory without executing tests:

```bash
python3 scripts/validation-profile.py
```

Add `--execute` to record wall time for each compiled test binary under the
ordinary `--skip dst_` filter. The profiler writes JSON beside lane reports;
Cargo's detailed compile/link breakdown remains in `target/cargo-timings/`.

## Updating the contract

When adding or renaming a workspace package, assign it to exactly one ordinary
test lane. When adding a required CI check, add a manifest lane and reference
its identifier from `.github/workflows/ci.yml`. Validate both invariants with:

```bash
python3 scripts/validation.py check
python3 scripts/test-validation.py
```

The contract check rejects duplicate or omitted workspace packages, unknown
categories, malformed commands, unsafe worker counts, and required lanes that
CI does not reference.
