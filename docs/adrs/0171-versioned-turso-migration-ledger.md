# ADR-0171: Versioned Turso migration ledger

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ADR-0066: Storage stack backend selection
  - ADR-0068: Turso write-gate retrospective
  - ADR-0074: Turso-Postgres ETL methodology
  - ARN-242: Turso startup schema changes swallow migration errors and have no version ledger
  - `crates/temper-store-turso/src/store/mod.rs`
  - `crates/temper-store-turso/src/router.rs`

## Context

`TursoEventStore::new` currently runs one ad hoc sequence of `CREATE` and
`ALTER` statements on every connection. Some failures propagate, some are
classified by matching error-message text, and several groups discard every
error. The sequence also creates `tenant_secrets` twice. Consequently, an
unsupported statement, conflicting schema object, permission failure, or
interrupted partial upgrade can be treated as a successful startup. The first
visible failure then occurs later on an unrelated data path, after the store has
already been admitted for traffic.

The sequence records neither which changes committed nor which definition was
used. Two processes can also inspect and mutate the same unversioned schema at
the same time. `TenantStoreRouter` adds a second schema owner by creating its
platform tables after `TursoEventStore` reports ready.

Temper needs a startup contract that can distinguish a fresh database, every
supported legacy shape, an interrupted upgrade, a changed historical migration,
and a database written by newer code. Readiness must mean that the complete
schema required by the current binary has been verified.

## Decision

### Sub-Decision 1: One append-only migration catalog owns all Turso schema

The Turso store will define an ordered, contiguous catalog of immutable
migrations. The catalog includes the entity-store schema and the router's
platform tables, eliminating `TenantStoreRouter::migrate_platform` as a second
owner.

The first catalog release groups the existing schema history into ordered
capability migrations: journal/snapshots, specs and integrations,
authorization/artifacts, installed apps and platform metadata,
trajectory/OTS extensions, blob/query-plane projections, and declared
key/vector indexes. Every future schema change appends a new version; released
migration definitions are never edited or reordered.

**Why this approach**: a catalog makes upgrade order explicit without dropping
support for databases created by the former ad hoc sequence. Keeping all Turso
DDL behind the same runner gives every store instance the same readiness
contract.

### Sub-Decision 2: Successful versions are recorded with content checksums

The runner bootstraps one metadata table:

```sql
CREATE TABLE IF NOT EXISTS temper_schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL UNIQUE,
    checksum TEXT NOT NULL CHECK (length(checksum) = 64),
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

Each checksum is SHA-256 over the complete canonical migration definition: the
version, name, ordered operation kinds, object identifiers, SQL, conditional
application metadata, and every declared post-migration capability. Capability
manifests use an explicit versioned, length-prefixed field encoding over ordered
maps and sets; compiler debug formatting and serializer implementation details
never enter the durable checksum. A change to either mutation behavior or
compatibility validation therefore changes the checksum. Startup validates the
complete ledger before applying work:

- versions must be positive, unique, contiguous, and known to this binary;
- recorded names and checksums must exactly match the compiled catalog;
- a ledger version newer than the binary is an incompatible schema, not a
  downgrade opportunity;
- a later recorded version with an earlier gap is corruption and prevents
  readiness.

**Why this approach**: version alone cannot detect a historical migration that
was edited in place. Checksums make the append-only rule executable.

### Sub-Decision 3: Migration application and ledger insertion are atomic

For each pending version, the runner starts an immediate transaction, rereads
that version's ledger row inside the transaction, applies the migration, verifies
the migration's schema capabilities, inserts the ledger row, and commits. Any
error rolls back both DDL and ledger insertion. The in-transaction reread lets
independent processes race safely: after one commits, the next observes and
validates the recorded checksum rather than replaying the work.

Local connections retain WAL and busy-timeout configuration before the runner
starts. The busy handler is installed before WAL initialization so concurrent
fresh opens can wait during journal-mode setup. Remote Turso connections use the
same libSQL transaction boundary but skip local-only PRAGMAs.

**Why this approach**: SQLite/libSQL DDL is transactional. Coupling schema work
and its durable acknowledgement removes the partial-success state that the
current startup path permits.

### Sub-Decision 4: Legacy reconciliation uses exact introspection

Legacy databases have schema objects but no ledger. Catalog operations therefore
remain convergent:

- tables and indexes use idempotent creation after confirming that an existing
  object of the same name has the required object kind and declared semantics;
- a column is added only when `pragma_table_info` proves it is absent;
- an existing required column is accepted without issuing `ALTER TABLE` only
  when its declared type affinity, nullability, default expression, and primary
  key ordinal match;
- declared unique keys and foreign keys must match their ordered columns,
  targets, and actions; named indexes must match their owner, uniqueness,
  ordered key columns, collation/sort direction, and partial predicate;
- the one legacy OTS shape that cannot add its non-constant timestamp default
  in place is rebuilt only after exact pre-upgrade column validation; explicit
  indexes and triggers are captured and recreated in the same transaction, while
  unmodeled columns, unique keys, or foreign keys prevent mutation;
- every other DDL failure propagates with migration version, name, operation,
  and object context.

There is no duplicate-column error-message matching. A view where a table is
required, a malformed table, or unsupported SQL is an incompatible schema and
fails startup.

**Why this approach**: exact schema state, not backend-specific prose in an error
message, determines whether an operation is already complete.

### Sub-Decision 5: Readiness includes final capability verification

After every migration and again after the full catalog, the runner checks all
required object kinds; column affinity, nullability, defaults, and primary-key
positions; unique/foreign-key semantics; named-index owners, uniqueness, key
ordering, collation/sort direction, and predicates; and the ledger head. These
capability declarations are part of the migration checksum. Completed catalogs
are reverified in prefix order so drift is attributed to the earliest migration
that owns the failed capability. A store is returned from
`TursoEventStore::new` only after that verification succeeds. Diagnostics
identify the migration version and the missing or incompatible capability so
operators can repair or restore the database without waiting for a later query
to fail.

**Why this approach**: a successful `CREATE TABLE IF NOT EXISTS` says only that
an object name exists. It does not prove that an old or manually modified object
can serve the current data paths.

## Rollout Plan

1. Ship the catalog, ledger, transactional runner, exact legacy reconciliation,
   router-schema consolidation, and the complete regression matrix together.
2. On first startup, fresh databases apply the catalog in order; legacy
   databases reconcile their existing objects and atomically record each version.
3. Refuse traffic on checksum mismatch, a newer ledger head, malformed legacy
   objects, or any unexpected DDL/verification failure.
4. Verify the exact startup flow locally with concurrent independent store
   instances and retain the commands/output on the pull request.

## Readiness Gates

- Fresh install records every catalog version and passes final verification.
- Every catalog prefix upgrades to the current head and a second startup is a
  no-op.
- A legacy no-ledger database and representative partial legacy schemas converge.
- Injected DDL/permission failure rolls back the active version and prevents
  `TursoEventStore::new` from returning a store.
- A checksum mismatch, ledger gap, or newer schema version prevents readiness
  with an actionable diagnostic.
- Concurrent independent startups produce one valid, contiguous ledger.
- Existing Turso event-store behavior remains green across the workspace.

## Consequences

### Positive

- Startup fails at the schema boundary instead of accepting mixed schemas.
- Operators can identify the exact migration catalog applied to a database.
- Legacy adoption, retries, and concurrent startup have one durable contract.
- Platform/router tables no longer have a separate migration path.

### Negative

- Startup performs bounded schema introspection and ledger validation.
- Historical migration definitions become immutable; correcting one requires a
  new compensating migration.
- An older binary cannot open a database whose ledger was advanced by newer code.

### Risks

- An incomplete capability manifest could admit a malformed legacy table.
  Mitigation: each migration declares and tests its required objects/columns,
  followed by a full-catalog verification.
- Remote libSQL transaction behavior could diverge from local SQLite.
  Mitigation: use only the transaction and introspection surfaces exposed by the
  shared `libsql` API and retain remote-compatible SQL.
- Long-running concurrent startup could exhaust the configured busy timeout.
  Mitigation: migrations are bounded, transactions cover one version at a time,
  and lock errors fail readiness rather than being swallowed.

### DST Compliance

This decision changes only `temper-store-turso` startup persistence code. It does
not touch the simulation-visible `temper-runtime`, `temper-jit`, or
`temper-server` crates. The concurrency regression uses independent database
connections and durable ledger assertions; no simulation exception is required.

## Non-Goals

- Changing the Postgres migration system or Redis storage.
- Redesigning domain tables or backfilling domain data.
- Allowing automatic downgrade or checksum repair.
- Keeping the former best-effort startup path as a compatibility fallback.

## Alternatives Considered

1. **Keep the current sequence and classify more errors** — rejected. Error text
   is not a durable migration record, cannot detect edited history/newer schemas,
   and still permits partial upgrades.
2. **Use only `PRAGMA user_version`** — rejected. It stores one integer with no
   per-migration checksum, diagnostic history, or atomic proof for each change.
3. **Adopt an external migration framework** — rejected for this bounded backend.
   The required ordering, checksums, introspection, and transactions fit behind a
   small store-local runner without adding a second database abstraction.
4. **Baseline every existing database as current** — rejected. That would record
   success without proving missing columns/indexes and preserve the original bug.

## Rollback Policy

Do not delete or rewrite ledger rows. Before production migration, rollback is a
normal binary rollback. After a database records a version unknown to the older
binary, restore a pre-migration database snapshot or deploy a forward
compensating migration with a compatible binary. Domain data is never discarded
to force a downgrade.
