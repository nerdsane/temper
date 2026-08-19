# ADR-0168: One crate for Temper DST suites

- Status: Accepted
- Date: 2026-08-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0016: Verification cascade hardening (L2b still unwired; [ARN-365](https://linear.app/arni-build/issue/ARN-365))
  - ADR-0017: Platform-level deterministic simulation testing
  - ADR-0032: PlatformStore trait and simulation-level platform DST
  - ADR-0079: CI PR smoke and DST matrix
  - `crates/temper-runtime/src/scheduler/` (engine)
  - `crates/temper-store-sim/` (fake journal)
  - `crates/temper-server/src/entity_actor/sim_handler.rs` (handler)

## Context

Deterministic simulation of Temper was split across crates in a way that matched dependencies but not humans or CI:

- Engine: `SimActorSystem` in `temper-runtime`
- Fake journal: `temper-store-sim`
- Handler: `EntityActorHandler` in `temper-server`
- Suites: `temper-server/tests/dst_*.rs`, `temper-platform/tests/system_entity_dst.rs`, plus a unit-test DST under `odata/query_plane_read`
- CI: `cargo test --workspace -- --skip dst_` then a matrix of `--test dst_*` on `temper-server`

That skip is a name prefix, not a home. `system_entity_dst` test names have no `dst_`, so they ran in the ordinary job. `dst_entity_key_index` and `dst_projection_lag` were skipped and not in the matrix.

The engines cannot live in one crate: `temper-verify` must not depend on `temper-server`; `temper-runtime` must not depend on the actor. The **suites** can.

## Decision

### Sub-Decision 1: Suite crate `temper-dst`

Add `crates/temper-dst`. It is the only place that means “prove Temper under a seed.” Production binaries must not depend on it (`publish = false`).

It depends on `temper-server`, `temper-platform`, `temper-runtime`, `temper-store-sim`, `temper-jit`, `temper-spec`.

**Why this approach**: One `cargo test -p temper-dst`. One CI job name. Helpers stop living in `temper-server/tests/common` next to OData tests.

### Sub-Decision 2: Engines stay

Do not move `SimActorSystem`, `SimEventStore`, `EntityActorHandler`, or verify L2 `simulation.rs`. Serve and (later) cascade L2b call those from production crates.

**Why this approach**: Already decided. EntityActor stays in `temper-server`. L2b helper, when wired ([ARN-365](https://linear.app/arni-build/issue/ARN-365)), lives next to `EntityActorHandler` because deploy cannot depend on a test crate.

### Sub-Decision 3: Server tests re-export helpers

Non-DST server tests (`odata_read`, GEPA, passivation) keep `mod common` and get the same helpers via `pub use temper_dst::…`.

**Why this approach**: Avoid a second copy of `build_default_state`.

### Sub-Decision 4: What does not move

- `temper-platform/tests/platform_e2e.rs` — in-memory E2E, not DST
- `reference-apps/*/tests/*_dst.rs` — app specs; they may call `temper-dst` helpers later
- `temper-server/src/odata/query_plane_read/tests/dst_projection_lag.rs` — uses crate-private storage types. Stays a server unit test until those types are pub. CI still runs it in the DST job by filter.

## Rollout Plan

1. **Phase 0 (this PR)** — Create crate, move helpers and suites, point CI at `temper-dst`, keep projection-lag filter on server.
2. **Phase 1** — [ARN-365](https://linear.app/arni-build/issue/ARN-365) wire L2b using a server-side helper, not this crate.
3. **Phase 2** — Optionally move projection-lag once the query-plane traits it needs are public.

## Consequences

### Positive
- “Where is Temper DST?” has one folder.
- CI cannot drop a suite by forgetting a `--test` flag.
- `--skip dst_` is no longer the only way to keep the workspace job short (`--exclude temper-dst`).

### Negative
- One more workspace member.
- Server integration tests take a dev-dependency on `temper-dst` (lib → server, server tests → lib; allowed).

### Risks
- Pre-push `cargo test --workspace` now includes `temper-dst` and is slow. Same as today when people did not pass `--skip dst_`. Mitigation: document `--exclude temper-dst` for the fast path; CI already splits the suite.

### DST Compliance
- No production path change. Helpers keep `BTreeMap`, `sim_now` / `sim_uuid`, seeded stores.

## Non-Goals

- Wiring cascade L2b
- Moving EntityActor
- Moving app DST out of `reference-apps`
- Making query-plane types public so projection-lag can move

## Alternatives Considered

1. **`temper-server/tests/dst/` only** — No new crate. Rejected: people will still say “DST is the server crate.”
2. **Move engines into `temper-dst`** — Rejected: serve cannot depend on a test crate; handler stays next to EntityActor.

## Rollback Policy

Delete `crates/temper-dst` and `git mv` the test files back. Helpers return to `temper-server/tests/common`.
