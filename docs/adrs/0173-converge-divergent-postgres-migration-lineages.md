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

| Version | Fork lineage | Legacy upstream lineage | Corrected upstream lineage |
| --- | --- | --- | --- |
| `0012` | Entity vector index | Evolution tenant ownership | Entity vector index |
| `0013` | Scoped schema deployments | Trajectory session index | Trajectory session index |
| `0014` | Not assigned | Trajectory capture sequence | Trajectory capture sequence |
| `0015` | Not assigned | OTS trajectory tenant identity | OTS trajectory tenant identity |
| `0016` | Historical union convergence | Historical union convergence | Evolution tenant ownership |

SQLx identifies an applied migration by numeric version and checksum. A merged flat migration
directory therefore cannot represent both histories: it contains duplicate versions on a fresh
database, and choosing either checksum makes an existing database from the other lineage fail
validation. Renaming historical files alone is also unsafe because deployed databases retain the
original version/checksum pairs.

Upstream merge `724eda61` removed its duplicate `0012` by publishing evolution tenant ownership as
`0016`. That fixes upstream's flat migrator but creates a second immutable meaning for `0016`: fork
and legacy-upstream databases may already record the union convergence checksum there. The
corrected upstream stream also shares the fork's `0012` checksum and becomes distinguishable only
when `0013` is recorded.

The merged kernel must upgrade fork databases, upstream databases, partially upgraded databases,
and fresh databases to one schema without rewriting trusted migration history or silently accepting
an unknown lineage.

## Decision

### Preserve three immutable pre-shared streams

The migration runner classifies an existing database from the recorded checksums in
`_sqlx_migrations`. The known fork, legacy-upstream, and corrected-upstream version/checksum
sequences are embedded as separate immutable streams. Versions `0001` through `0011` remain the
common prefix.

- A database with no divergent migration uses the fork stream as the canonical fresh-install path.
- A checksum matching legacy upstream's `0012` selects the legacy-upstream stream.
- Fork and corrected upstream share `0012`; their distinct `0013` checksums select the stream.
- A database interrupted after the shared `0012` has no distinguishable intent and safely resumes
  on the canonical fork stream. Shared convergence produces the same final schema.
- Version `0016` must match the historical convergence checksum for fork/legacy-upstream histories
  or the evolution-ownership checksum for corrected-upstream histories.
- Partial histories are completed only by the stream selected by their first divergent checksum.
- Unknown checksums, cross-lineage mixtures, failed migration rows, gaps, or contradictory records
  fail before any schema mutation.

**Why this approach**: each deployed history remains verifiable using the exact files that produced
it. Classification is based on SQLx's cryptographic migration identity rather than mutable table or
column heuristics.

### Preserve `0016` and apply one shared stream at `0017`

The historical convergence migration at `0016` remains byte-for-byte available to fork and
legacy-upstream databases; it cannot be renamed because deployed databases record its identity.
Corrected-upstream databases instead retain upstream's evolution-ownership `0016` unchanged.

After the selected pre-shared stream is complete, a new migration namespace beginning at `0017`
applies the union of all schema changes. The first shared migration contains idempotent DDL and
bounded backfills for:

- entity vector indexes;
- scoped schema deployments;
- evolution tenant ownership;
- ordered trajectory session capture; and
- tenant-scoped OTS trajectory identity.

The same `0017` migration runs after every pre-shared stream and on fresh installs. Every later
PostgreSQL migration uses the single shared sequence beginning at `0018`.

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

1. Preserve the exact fork, legacy-upstream, corrected-upstream, and historical-convergence sources
   in separately embedded streams.
2. Extend the lineage classifier and selected-stream runner without weakening fail-closed checks.
3. Add shared migration `0017` with the idempotent union schema and data backfills.
4. Exercise a restart after every migration boundary of every valid stream, plus mixed,
   unknown-checksum, gapped, and failed-row histories against real PostgreSQL.
5. Ship the runner and convergence migration together; there is no intermediate deployable state.

## Readiness Gates

- Fresh installation produces the complete union schema.
- Fork and upstream snapshots converge without editing their existing `_sqlx_migrations` rows.
- Interrupted upgrades resume within their selected lineage; the checksum-ambiguous shared `0012`
  boundary resumes through the canonical fork path.
- A second startup performs no schema or migration-history writes.
- Mixed, unknown, gapped, and failed histories are rejected before mutation.
- PostgreSQL backend parity, restart, and schema cutover tests pass alongside Turso, Redis, and
  simulation tests.

## Consequences

### Positive

- All three deployed histories remain upgradeable and auditable.
- Fresh and upgraded databases reach the same schema and future migration sequence.
- Migration ambiguity becomes an explicit startup error rather than a partial deployment.

### Negative

- Historical migration sources exist in three immutable pre-shared streams and must remain
  available.
- The migration runner is more complex than one unconditional `sqlx::migrate!().run()` call.
- Convergence DDL intentionally repeats already-applied operations using idempotent guards.

### Risks

- An incomplete classifier could select the wrong stream. Exact checksum and sequence validation,
  plus fail-closed fixtures for malformed histories, mitigate this.
- An allegedly idempotent backfill could overwrite newer data. Convergence backfills update only
  rows that lack the new value and are covered by old-or-new cutover tests.
- Future contributors could reuse a historical version, including by changing zero-padding. CI
  parses and normalizes numeric prefixes, asserts uniqueness within every active migrator, and
  reserves all versions through the shared `0017` boundary.

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

Before the lineage-specific `0016` commits, startup failure leaves the database on its selected
lineage and the corresponding previous binary remains usable. The two `0016` identities are never
interchangeable. After shared migration `0017` commits, rollback may use only a binary that both
recognizes the database's selected `0016` checksum and tolerates the additive union schema. Applied
records at `0016` and `0017` must remain intact. Destructive down migrations and migration-history
edits are prohibited; any defect discovered after convergence requires a forward corrective
migration at `0018` or later.
