---
name: verify-temper
description: Launch the Temper kernel locally and verify a change end to end - build, serve, drive the OData surface, run the verification cascade and DST proof. Use before calling any temper change done.
---

# Verify temper

## Launch

```bash
cargo build -p temper-cli                                    # first build is long (~29 crates)
cargo run -p temper-cli -- serve --port 3600                 # pick a free port; capture the PID
```

Ready when `GET http://localhost:3600/healthz` returns 200 (unauthenticated liveness; `/observe/health` is behind Observe auth and 401s) and `GET /tdata/$metadata` with `X-Tenant-Id: default` returns CSDL XML.

For authenticated entity reads and dispatches, set `TEMPER_API_KEY=<any local value>` in the environment BEFORE serve - the platform bootstraps a tenant credential from it at startup, and a keyless boot serves 401 on every governed route (that 401 is itself the fail-closed proof).

**ISOLATE**: run from your worktree so state lands in the worktree, not in a shared checkout. Never point at another session's data directory.

## Doctor

- Build fails on `edition 2024`: rustup update; rust-version is 1.85.
- Port in use: pick another; read the real port from the serve log, not the flag you passed.
- Serve exits immediately: read the log bottom-up; a spec that fails the L0-L3 cascade at bootstrap names itself.

## Verify a change

Pick the feature file matching what changed (see `features/`):

- `features/serve-and-odata.md` - boot, health, CSDL metadata, entity reads
- `features/spec-cascade.md` - L0-L3 verification of `.ioa.toml` changes
- `features/dst-proof.md` - deterministic simulation, seeded reproduction

Always finish with the suite for the crates you touched (`cargo test -p <crate>`), then `cargo test --workspace` before push (the pre-push hook runs it anyway).

## Evidence

Capture into `/tmp/verify-temper/<date>/`: the health response, the metadata head, cascade output, and the DST test result. Hand commands + outputs to the PR, do not assert.

## Teardown

Kill only the serve PID you captured at spawn. Never kill by pattern - other temper worktrees run servers on this machine.
