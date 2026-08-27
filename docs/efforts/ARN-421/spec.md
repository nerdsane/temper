# Spec / RFC — ARN-421: Genesis app-install is source of truth (verify + rollback + env-pin-as-floor)

One contract, three expressions (temper rule): this spec.md, the DST invariant **P18** in
`crates/temper-server/tests/common/platform_invariants.rs`, and the scenario
`crates/temper-server/tests/dst_genesis_install_rollback.rs`. All three say the same thing; if one
changes, all change.

## Vocabulary
- **pinned ref** — the env `TEMPERPAW_GENESIS_BOOTSTRAP_REFS` entry `owner/app@hash`. A cold-start seed.
- **runtime-ready** — `recover_installed_app_runtime_state` returns `Ready` or `Healed` (specs ready +
  policies active + required wasm registered) AND every app-required wasm module compiles.
- **prior record** — the `InstalledAppRecord` for the app that existed before this install began.

## Gap 1 — env bootstrap pin is a floor, not a ceiling (temperpaw)
`bootstrap_configured_genesis_apps` must keep any runtime-ready Genesis install regardless of its
hash, and (re)install the pinned ref only when there is nothing healthy to keep.

Decision table (pure fn `classify_bootstrap_action(record, genesis_runtime_ready)`):

| installed record                                   | runtime-ready | action         |
|----------------------------------------------------|---------------|----------------|
| none                                               | —             | InstallPinned  |
| source_kind != genesis                             | —             | InstallPinned  |
| genesis, status != "installed"                     | —             | InstallPinned  |
| genesis, status == "installed"                     | false         | InstallPinned  |
| genesis, status == "installed"                     | true          | KeepInstalled  |

Consequence (intended): bumping the env pin forward no longer force-upgrades a healthy older install.
Explicit install (agent tool / endpoint) is the upgrade path. This is what makes a Genesis/agent
publish authoritative over a stale env pin.

## Gap 2 — install is verified-or-reverted (temper kernel)
`install_genesis_app_from_registry` materializes the pinned closure (network/git — NOT simulatable),
then delegates the reconcile+record to a post-materialization helper that is verified-or-reverted and
is the DST-simulatable seam (local catalog).

Pure decision fn `classify_install_verify(new_version_ready, prior_record) -> InstallVerifyDecision`:

| new_version_ready | prior genesis+installed record | decision            |
|-------------------|--------------------------------|---------------------|
| true              | —                              | Commit              |
| false             | present                        | RollBackToPrevious  |
| false             | absent                         | FailNoRollback      |

Effects:
- **Commit** — record the new provenance; return success.
- **RollBackToPrevious** — restore the prior `InstalledAppRecord` and re-reconcile the prior bundle
  (re-materialize the prior pinned ref if its cache was evicted), re-verify it reaches runtime-ready,
  then surface the original failure as `Err`. If the prior ref ALSO fails to verify → hard
  both-broken error.
- **FailNoRollback** — surface `Err`, mark the `AppInstallation` entity failed; nothing to revert to.

Verification (B2): `recover_installed_app_runtime_state` == Ready|Healed, AND every **app-required**
wasm module (declared in the bundle's `app.toml`, not stray/optional `.wasm`) compiles via
`WasmEngine::compile_and_cache`. Eager-compiling at install turns "failed to compile lazy-loaded WASM
module" into an install-time rollback trigger instead of a first-load prod break. Live/Datadog health
remains ARN-420's outer layer; the kernel owns readiness + compile.

## Invariant P18 (the formal expression)
For every simulated tenant, after any install attempt: the tenant's durable `InstalledAppRecord` and
its in-memory spec/policy/wasm registration reflect a runtime-ready version — the newly committed one
if it verified, otherwise the prior good version — never a partially-applied new digest. An install
that never reaches Ready leaves the tenant pinned to the previous good digest. P18 must FAIL against
`main` (no rollback today) and PASS after the fix; the failing seed is committed as a regression case.

## Out of scope (deferred, see decisions.md D5)
follow-latest authority (Genesis `LatestVersionHash` auto-authoritative for `follow_latest` apps).
No `follow_latest` app is bootstrapped today; deferred as speculative.
