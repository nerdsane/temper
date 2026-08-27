# Plan — ARN-422

## What we are addressing
Two gaps that break "Genesis is the source of truth for installs": (1) a redeploy downgrades a newer
install to the env pin; (2) the kernel install has no verify and no rollback.

## Expected end state
- temperpaw: `bootstrap_configured_genesis_apps` keeps any runtime-ready Genesis install; pure
  `classify_bootstrap_action` unit-tested. [DONE]
- temper kernel: `install_genesis_app_from_registry` delegates reconcile+record to a
  verified-or-reverted post-materialization helper; pure `classify_install_verify`; B2 verification;
  rollback restores prior record + re-reconciles prior bundle. DST invariant P18 + seeded regression.
- ADR in temper `docs/adrs/`. One draft PR per repo. Merge order Temper → TemperPaw (pin bump bundled
  into the temperpaw PR after temper merges).

## Steps
1. [DONE] Gap 1 temperpaw: decision fn + red-green unit test; fmt/clippy clean; committed.
2. Gap 2 harness-first: add P18 to `platform_invariants.rs` + scenario
   `dst_genesis_install_rollback.rs` that installs a good version, attempts a version that fails
   verification (fault-injected non-ready reconcile), asserts P18. Confirm it FAILS on main.
3. Gap 2 impl: extract post-materialization helper + `classify_install_verify` (pure, unit-tested) +
   B2 compile probe over app-required modules; wire `install_genesis_app_from_registry` to it; wire a
   harness method to drive the same helper. Confirm P18 PASSES; sweep many seeds; commit failing seed.
4. ADR-XXXX (temper): "Genesis install is verified-or-reverted; env pin is a floor."
5. Push branches as rita-aga; open both draft PRs with `## Decisions & Tradeoffs` verbatim.
6. Review panel (Grok, Codex, Fable) + Greptile per PR; fix everything; re-verify.
7. Live e2e (Definition of Done):
   - Publish an app version to Genesis; install via `/paw/apps/install-from-genesis`; verify live.
   - Simulate a bad publish (module that fails wasip1/compile); confirm install rolls back to
     last-good; app stays serving the prior version.
   - Redeploy with the env pin behind the installed version; confirm bootstrap KEEPS the newer install.
   - Capture commands + output.
8. Merge Temper; bump temperpaw temper-pin to the merged commit; merge TemperPaw; verify live on
   Railway (installed pinned ref `owner/app@hash`), Datadog no No-Data / APM hits.

## Risks
- Rollback re-materialization needs network in prod if the prior cache was evicted; DST covers the
  local re-reconcile, not the network fetch (documented boundary).
- Eager wasm compile adds install latency; scoped to app-required modules to bound it.
