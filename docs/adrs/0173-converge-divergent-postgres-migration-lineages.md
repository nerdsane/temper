# ADR-0173: Converge Divergent PostgreSQL Migration Lineages

- Status: Accepted
- Date: 2026-08-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0156: Immutable Typed Cross-Entity References
  - ADR-0158: Durable Observable Entity Reactions
  - ADR-0159: Task-Scoped Schema Deployment
  - `crates/temper-store-postgres/src/migration.rs`
  - `crates/temper-store-postgres/migrations/`

## Context

The Temper fork and upstream continued from the same PostgreSQL migration history through version
`0011`, then independently assigned the same SQLx migration versions to different schema changes:

| Version | Fork lineage | Upstream lineage |
| --- | --- | --- |
| `0012` | Entity vector index | Evolution tenant ownership |
| `0013` | Scoped schema deployments | Trajectory session index |
| `0014` | Not assigned | Trajectory capture sequence |
| `0015` | Not assigned | OTS trajectory tenant identity |

SQLx identifies an applied migration by numeric version and checksum. A merged flat migration
directory therefore cannot represent both histories: it contains duplicate versions on a fresh
database, and choosing either checksum makes an existing database from the other lineage fail
validation. Renaming historical files alone is also unsafe because deployed databases retain the
original version/checksum pairs.

The merged kernel must upgrade fork databases, upstream databases, partially upgraded databases,
and fresh databases to one schema without rewriting trusted migration history or silently accepting
an unknown lineage.

## Decision

### Preserve two immutable legacy streams

The migration runner classifies an existing database from the recorded checksums in
`_sqlx_migrations`. The known fork and upstream version/checksum sequences are embedded as separate,
immutable legacy streams. Versions `0001` through `0011` remain the common prefix.

- A database with no divergent migration uses the fork stream as the canonical fresh-install path.
- A checksum matching the fork's `0012` selects the fork stream.
- A checksum matching upstream's `0012` selects the upstream stream.
- Partial histories are completed only by the stream selected by their first divergent checksum.
- Unknown checksums, cross-lineage mixtures, failed migration rows, gaps, or contradictory records
  fail before any schema mutation.

**Why this approach**: each deployed history remains verifiable using the exact files that produced
it. Classification is based on SQLx's cryptographic migration identity rather than mutable table or
column heuristics.

### Apply one idempotent convergence stream

After the selected legacy stream is complete, a new migration namespace beginning above every
historical version applies the union of both lineages' schema changes. The first convergence
migration uses version `0016` and contains idempotent DDL and bounded backfills for:

- entity vector indexes;
- scoped schema deployments;
- evolution tenant ownership;
- ordered trajectory session capture; and
- tenant-scoped OTS trajectory identity.

The same convergence migration runs after either legacy stream and on fresh installs. Every later
PostgreSQL migration uses a single shared sequence beginning at `0017`.

**Why this approach**: an identical post-convergence migration identity gives all databases one
future history while idempotent union DDL makes the operation safe regardless of which side already
exists.

### Never rewrite applied migration records

The runner may read `_sqlx_migrations`, validate rows, and append successful migrations. It must not
delete, renumber, edit, or replace an applied migration record. It must not mark a migration applied
without executing its SQL through SQLx's transactional migration machinery.

**Why this approach**: rewriting migration metadata would erase the evidence needed to distinguish
lineages and could claim schema changes that never ran.

### Fail closed before mutation

Classification and complete-history validation happen before any pending migration executes. Error
messages identify the conflicting version and whether the history is unknown, mixed, failed, or
incomplete. They do not expose connection credentials or migration contents.

**Why this approach**: choosing a lineage heuristically after mutation could leave the database in a
third, unsupported state.

## Rollout Plan

1. Preserve the exact fork and upstream migration sources in separately embedded legacy streams.
2. Add the lineage classifier and selected-stream runner.
3. Add migration `0016` with the idempotent union schema and data backfills.
4. Exercise fresh, common-prefix, partial-fork, complete-fork, partial-upstream, complete-upstream,
   converged, mixed, unknown-checksum, and failed-row histories against real PostgreSQL.
5. Ship the runner and convergence migration together; there is no intermediate deployable state.

## Readiness Gates

- Fresh installation produces the complete union schema.
- Fork and upstream snapshots converge without editing their existing `_sqlx_migrations` rows.
- Interrupted upgrades resume within their selected lineage.
- A second startup performs no schema or migration-history writes.
- Mixed, unknown, gapped, and failed histories are rejected before mutation.
- PostgreSQL backend parity, restart, and schema cutover tests pass alongside Turso, Redis, and
  simulation tests.

## Consequences

### Positive

- Both deployed histories remain upgradeable and auditable.
- Fresh and upgraded databases reach the same schema and future migration sequence.
- Migration ambiguity becomes an explicit startup error rather than a partial deployment.

### Negative

- Historical migration sources exist in two immutable streams and must remain available.
- The migration runner is more complex than one unconditional `sqlx::migrate!().run()` call.
- Convergence DDL intentionally repeats already-applied operations using idempotent guards.

### Risks

- An incomplete classifier could select the wrong stream. Exact checksum and sequence validation,
  plus fail-closed fixtures for malformed histories, mitigate this.
- An allegedly idempotent backfill could overwrite newer data. Convergence backfills update only
  rows that lack the new value and are covered by old-or-new cutover tests.
- Future contributors could reuse a historical version. CI will assert uniqueness within each
  stream and reserve all versions through `0016`.

### DST Compliance

The migration classifier and SQL execution live in `temper-store-postgres`, outside simulation
state. Tests use fixed migration histories and compare deterministic schema/history snapshots. No
clock, RNG, actor scheduling, or simulation-visible iteration behavior changes.

## Non-Goals

- Renumbering or deleting records already applied in production.
- Supporting an unrecognized private migration lineage automatically.
- Combining PostgreSQL and Turso migration mechanisms.
- Resolving the inherited duplicate ADR numbers from the merged documentation histories.

## Alternatives Considered

1. **Keep the fork numbering and rename upstream migrations** — Rejected because upstream databases
   already record different checksums for versions `0012` and `0013`.
2. **Keep the upstream numbering and rename fork migrations** — Rejected for the symmetric failure
   on fork databases.
3. **Rewrite `_sqlx_migrations` into one preferred history** — Rejected because it destroys audit
   evidence and can claim SQL was applied when it was not.
4. **Infer lineage from table or column presence** — Rejected because partial/manual schema changes
   are not cryptographic migration identity and can produce ambiguous classifications.
5. **Create a new baseline and require database replacement** — Rejected because it drops working
   upgrade capability and violates the requirement to preserve existing installations.

## Rollback Policy

Before migration `0016` commits, startup failure leaves the database on its original lineage and the
previous binary remains usable. After `0016` commits, rollback may use any binary that tolerates the
additive union schema, but the applied convergence record must remain intact. Destructive down
migrations and migration-history edits are prohibited; a forward corrective migration is required
for any defect discovered after convergence.
