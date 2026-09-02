---
name: verify-temper
description: Launch the Temper kernel locally and verify a change end to end - build, serve, drive the OData surface, run the verification cascade and DST proof. Use before calling any temper change done.
---

# Verify temper

## Launch

```bash
cargo build -p temper-cli                                    # first build is long (~29 crates)

# ISOLATE per instance: TURSO_URL unset defaults to the SHARED
# ~/.local/share/temper/agents.db (another session's real state; two servers on
# it corrupt each other). Always point at a per-worktree file. AND unset
# TURSO_PLATFORM_URL - bootstrap.rs checks it BEFORE TURSO_URL (serve/bootstrap.rs:59
# vs :90), so if it is set in your env, TURSO_URL is ignored and you hit the shared/
# cloud db instead.
unset TURSO_PLATFORM_URL
mkdir -p .scratch                                           # the turso file's dir must exist
TEMPER_API_KEY=local-verify \
TURSO_URL="file:$PWD/.scratch/temper.db" \
  cargo run -p temper-cli -- serve --port 3600 --storage turso   # capture the PID
```

`--storage turso` is the local libSQL backend (the default; `postgres`/`redis` are the other backends). The turso URL resolution is in `crates/temper-cli/src/serve/bootstrap.rs` (`TURSO_URL`, else the shared default path).

Ready when `GET http://localhost:3600/healthz` returns 200 (unauthenticated liveness; `/observe/health` is behind Observe auth and 401s) and `GET /tdata/$metadata` with `X-Tenant-Id: default` returns CSDL XML.

For authenticated entity reads and dispatches, set `TEMPER_API_KEY=<any local value>` in the environment BEFORE serve - the platform bootstraps a tenant credential from it at startup, and a keyless boot serves 401 on every governed route (that 401 is itself the fail-closed proof).

## Doctor

- Build fails on `edition 2024`: rustup update; rust-version is 1.85.
- Port in use: pick another; read the real port from the serve log, not the flag you passed.
- Serve exits immediately: read the log bottom-up; a spec that fails the cascade at bootstrap names itself.
- Boot hangs / stack overflow at startup: you are almost certainly sharing the turso db - set a unique `TURSO_URL`.

## Verify a change

Pick the feature file matching what changed (see `features/`); a proof that drives one convenient entry point is incomplete when the map lists others. Always finish with the suite for the crates you touched (`cargo test -p <crate>`), then `cargo test --workspace` before push (CI runs the full suite, the DST matrix and the readability ratchet on every PR; there is no pre-push hook).

## Evidence

Capture into `/tmp/verify-temper/<date>/`: the health response, the metadata head, cascade output, and the DST test result. Hand commands + outputs to the PR, do not assert.

## Teardown

Kill only the serve PID you captured at spawn. Never kill by pattern - other temper worktrees run servers on this machine. Remove the `.scratch/` db if you want a clean slate; leave everything else.
