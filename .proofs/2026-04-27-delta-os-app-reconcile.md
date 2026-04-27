# Delta OS-App Reconcile Proof

Date: 2026-04-27

Worktree: `/Users/seshendranalla/Development/temper-worktrees/os-app-delta-reconcile`

## Red Tests

Added failing tests before implementation:

- `upsert_specs_and_commit_preserves_identical_spec_version`
- `upsert_wasm_module_preserves_version_for_identical_hash`
- `upsert_wasm_module_stores_artifact_outside_metadata_row`
- `test_reconcile_plan_for_wasm_only_digest_skips_unrelated_phases`
- `test_reconcile_os_app_repairs_spec_content_drift_despite_matching_digest`

Initial failures on `main` behavior:

- identical spec commit bumped version from `1` to `2`
- identical WASM upsert bumped version from `1` to `2`
- new WASM metadata row stored `7` inline bytes instead of `0`
- reconcile plan API did not exist yet
- matching installed bundle digest skipped hot reconcile even when live runtime
  spec content had drifted

## Verification Commands

```bash
cargo test -p temper-store-turso
```

Result: passed. `34` unit tests, `5` blob TTL e2e tests, and doctests passed.

```bash
cargo test -p temper-platform os_apps::mod_test
```

Result: passed. `52` OS-app platform tests passed.

```bash
cargo run -p temper-cli -- --help
```

Result: passed. The Temper CLI built and printed help.

## Local Server Smoke

Started a local Temper server with a temp Turso DB and the project-management app:

```bash
rm -f /tmp/temper-delta-reconcile-e2e.db /tmp/temper-delta-reconcile-e2e.db-wal /tmp/temper-delta-reconcile-e2e.db-shm
TURSO_URL=file:/tmp/temper-delta-reconcile-e2e.db \
  cargo run -p temper-cli -- serve --storage turso --no-observe --port 39876 --skill project-management
```

Observed:

- server listened on `http://0.0.0.0:39876`
- `curl -fsS http://127.0.0.1:39876/healthz` exited successfully
- project-management installed for tenant `default`

Queried the local DB:

```bash
sqlite3 /tmp/temper-delta-reconcile-e2e.db \
  "select app_name, length(bundle_digest)>0 as has_bundle, length(spec_digest)>0 as has_spec from tenant_installed_apps where tenant_id='default';
   select entity_type, version, committed from specs where tenant='default' order by entity_type;"
```

Observed:

- `tenant_installed_apps` had `project-management` with bundle and spec digests present
- committed specs for `default` included `Comment`, `Cycle`, `Issue`, `Label`, and `Project`
- project-management spec versions were `1` and committed

Stopped the server with `SIGTERM` and confirmed no matching process remained.

## Notes

The local smoke run is a cold install because it starts from an empty DB. Delta reconcile behavior is covered by the OS-app tests:

- `test_reconcile_os_app_delta_content_change_skips_specs` forces a changed bundle/content digest with matching spec/policy/WASM/seed digests and verifies reconcile does not classify or bootstrap specs.
- `test_install_plan_without_spec_phase_does_not_reclassify_specs` verifies the installer obeys a plan with the spec phase disabled.
- `test_reconcile_os_app_repairs_spec_content_drift_despite_matching_digest` verifies a matching installed bundle record does not hide drifted live spec content.

## OpenPaw Live E2E

Ran OpenPaw in a disposable worktree using patched local Temper crates via
Cargo `[patch]` entries, a disposable home, and a file-backed Turso DB.

Cold boot from an empty DB:

- `/readyz` returned HTTP `200`
- `phase_6b_os_app_reconcile`: `11,813ms`
- startup time to ready: `12,227ms`
- `wasm_modules`: `31` rows, `31` metadata-only rows, `31` blob artifacts,
  `15,272,520` bytes in blobs, min/max metadata version `1/1`
- `specs`: `default` had `32` committed specs, min/max version `1/1`;
  `temper-system` had `13` committed specs, min/max version `1/1`

Warm boot using the same DB and `TEMPERPAW_WASM_STARTUP_POLICY=load-only`:

- `/readyz` returned HTTP `200`
- all six startup apps logged `Skipped unchanged OS app`
- `phase_6b_os_app_reconcile`: `1,267ms`
- startup time to ready: `1,862ms`
- `wasm_modules` and `specs` row counts and versions stayed unchanged
