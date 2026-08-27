# ADR-0173: Genesis install is verified-or-reverted

- Status: Accepted
- Date: 2026-08-27
- Deciders: Temper core maintainers
- Related:
  - ARN-421 (effort), ARN-420 (deploy/verify/rollback pipeline), ARN-411 (SDLC loop epic)
  - `crates/temper-platform/src/genesis_install.rs` (install path)
  - `crates/temper-platform/src/recovery.rs` (`recover_installed_app_runtime_state`)
  - `crates/temper-server/tests/dst_genesis_install_rollback.rs` (invariant P18)

## Context

`install_genesis_app_from_registry` is the one semantic install operation all callers share — the
agent tool, the `/paw/apps/install-from-genesis` endpoint, and startup bootstrap. It materialized a
pinned closure and reconciled it into the runtime, but did nothing after: no check that the app
actually reached runtime-ready, and no way back if it did not. A bad publish therefore installed
cleanly and only failed later — the "failed to compile lazy-loaded WASM module" break that took prod
down for ~4 minutes — with the durable record already pointing at the broken version and no revert.

Reconcile already eager-compiles only modules flagged `startup_loading = eager`, and merely warns on
failure. Required modules that are lazy-loaded are never compiled at install, so a broken one is
invisible until first use.

## Decision

### 1. Verify after reconcile, roll back on failure

After reconciling the pinned closure, the install verifies the root app is runtime-ready. If it is
not, and a previous good Genesis install exists, the install restores that previous version and
returns an error; if there is no safe prior, it fails cleanly (the `AppInstallation` entity is marked
failed by the existing hook). The verify+rollback lives inside the shared install function, so every
caller inherits it.

The routing is a pure decision function, unit-tested in isolation:

```
classify_install_verify(new_version_ready, prior_record) ->
    Commit | RollBackToPrevious | FailNoRollback
```

A rollback target must itself be a Genesis install in the `installed` status — anything else is not a
safe last-good state.

**Why this approach**: one shared path means one place to get verify+rollback right. Putting the
decision in a pure function keeps the policy testable without a live system.

### 2. Verification = runtime-ready AND required-wasm compiles

Verification passes only when `recover_installed_app_runtime_state` reports `Ready`/`Healed` (specs,
policies, and required wasm registered) **and** every app-required wasm module actually compiles via
`WasmEngine::compile_and_cache`. Optional modules are not required to compile, so a stray or optional
`.wasm` never fails an otherwise-good install.

**Why this approach**: eager-compiling the required modules at install turns the exact prod failure
into an install-time rollback trigger instead of a first-load break. Live/Datadog health remains the
outer layer (ARN-420); the kernel owns readiness + compile.

### 3. Rollback restores the record and re-reconciles the prior bundle

`reconcile_os_app` overwrites the durable record with a local-provenance row, so rollback restores
the prior Genesis provenance record and re-reconciles the prior bundle (re-materializing the prior
pinned ref if its on-disk cache was evicted). If the prior version also fails to reach runtime-ready,
that is a hard both-broken error — neither version is serviceable and the operator must intervene.

## Deterministic simulation

New stateful behavior, so it is covered harness-first. Invariant **P18**
(`dst_genesis_install_rollback.rs`): after a failed install rolls back, the durable record is the
prior good Genesis record, never the partial local-provenance state reconcile left behind. The
invariant fails before the rollback effect exists (verified by stubbing the rollback call: the record
stays `source_kind = "local", app_ref = ""`) and holds across 64 seeds under heavy platform-store
faults. The network-free rollback core (`restore_prior_install`) is the simulated seam; the network
re-materialization of an evicted prior cache is a documented boundary the simulator does not cover.

## Consequences

- A bad publish reverts to the last-good version automatically instead of leaving a broken app.
- Install latency grows by the cost of compiling required wasm modules, bounded to required modules.
- Rollback needs the prior bundle; if its cache was evicted it is re-materialized from the registry
  (network). If that also fails, the error is surfaced rather than silently leaving a broken state.
