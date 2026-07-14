# ADR-0174: Turso Schema Migration Ledger

## Status

Accepted (2026-07-14)

(Numbered 0174: unique on this arena branch. Sibling efforts used 0162
(claude) and 0171 (codex); this record is the Grok-line ADR for ARN-242.)

## Context

`TursoEventStore::migrate()` re-ran the entire DDL script on every boot with
no record of what had been applied, and twelve `let _ = conn.execute(...)`
sites (nine direct, three loops — 41 ALTERs in all) discarded every failure. The intent was to tolerate benign
duplicate-column errors on idempotent re-runs — but the pattern equally
swallowed locked databases, disk errors, shadowed tables, and syntax errors,
so a genuinely failed migration left a half-migrated database that the
server then served against, silently (ARN-242).

## Decision

1. **Fail-closed idempotent execution.** `execute_idempotent` tolerates only
   the benign already-applied errors (duplicate column / already exists);
   everything else propagates and fails startup, with the failing statement
   in the message. All twelve former swallow sites (41 ALTERs) and the two
   bespoke match blocks route through it, and a debug pre-assertion enforces
   its ADD-COLUMN-only precondition — so the debug test suite passing is
   itself proof that all 41 satisfy it.
2. **A durable ledger, gated on a schema FINGERPRINT.**
   `temper_schema_migrations (version, name, fingerprint, applied_at)` is
   created first; a fully successful run stamps the declared
   `SCHEMA_FINGERPRINT` (a SHA-256 of the schema a fresh migrate produces)
   together with `SCHEMA_VERSION` as a human-readable label. **The boot gate
   is the fingerprint, not the version:** a stamped database skips the DDL
   only while its stored fingerprint equals the declared one. Updating the
   fingerprint is therefore the very act that makes a schema change reach
   existing databases — the contract cannot be satisfied without it. The
   comparison is stored-constant vs declared-constant (never the live
   schema), so a platform database's extra `migrate_platform` tables cannot
   cause spurious re-runs.
3. **The ledger migrates itself, un-gated.** The ledger table sits in FRONT
   of the gate, so it can never be gated by its own fingerprint: its `CREATE`
   and its own `ADD COLUMN`s run on every boot, through `execute_idempotent`.
   Without this, adding any column to the ledger would make the next
   statement — the gate SELECT — fail at prepare time with "no such column"
   on every database whose ledger predates it: a hard boot failure, and
   exactly the class this ADR exists to kill, reproduced inside the ledger.
   On a fresh database the ALTER is a tolerated duplicate-column no-op, so
   `sqlite_master` (and the fingerprint) is unchanged.
4. **The gate asks "ever migrated to this schema", not "is the latest row
   this schema"** (`SELECT EXISTS(… WHERE fingerprint = ?)`). A binary rolled
   back to an older schema then finds its own retained row and skips, instead
   of re-running the whole DDL on every boot for the duration of the
   rollback.
5. **The gate is SCHEMA-shaped: `migrate()` is DDL-only.** `SCHEMA_FINGERPRINT`
   hashes `sqlite_master`, so it is blind to any statement that does not change
   the schema. A data migration placed in `migrate()` — a backfill, a seed row,
   an `UPDATE … WHERE … IS NULL` — would run on fresh databases, leave the
   fingerprint unchanged, keep the test and CI green, and **be skipped forever
   on every stamped database**. Bumping `SCHEMA_VERSION` does not save it
   either, because the version is deliberately off the correctness path. A data
   migration therefore cannot rely on this gate: it needs its own gating row
   (e.g. a ledger row keyed by the migration's name) or a separate mechanism.
6. **The contract:** EVERY change to the ledgered DDL must update
   `SCHEMA_FINGERPRINT` (the `schema_fingerprint_matches_declared_version`
   test fails otherwise and prints the new value), and should bump
   `SCHEMA_VERSION`/`SCHEMA_VERSION_NAME` as the human-readable label.
   Correctness rests on the fingerprint alone; the version is a label. Platform-registry DDL lives in `router.rs::migrate_platform`, OUTSIDE
   the ledger — it runs every boot and must stay fail-closed; bumping the
   constant does nothing for it. **Invariant:** every migration statement
   must be idempotent AND safe to run concurrently with another booting
   server (two servers can both see an unstamped ledger and both run the
   DDL; today every statement is `CREATE … IF NOT EXISTS` or a
   benign-tolerated `ADD COLUMN`, and the stamp is `INSERT OR REPLACE` on the
   version primary key — atomic within its statement, so two booters stamping
   the same `(version, name, fingerprint)` converge on one identical row and
   no third booter can observe a gap and spuriously re-run). Note `REPLACE`
   DESTROYS the prior row for that version: when a schema change updates the
   fingerprint without bumping the version (the version is only a label), the
   older build's row is overwritten — which is precisely the rollback caveat
   in Decision 4, and the reason a rolled-back binary re-runs the DDL once
   before skipping. A future version that backfills data or issues a bare
   `CREATE` must serialize the migration explicitly.
7. **Ordering:** the ledger check runs after the connection PRAGMAs (WAL,
   busy_timeout) and fully drains its query rows — an undrained statement
   before WAL was configured held a read lock that deadlocked concurrent
   writers (caught by the existing projection test during development).

## Consequences

- A real migration failure now fails boot loudly instead of leaving a
  half-migrated schema in service; operators can read the ledger to see
  what version a database is at.
- Stamped boots skip the whole DDL script — 98 executed statements (56 direct
  `CREATE`s, 9 direct `ALTER`s, 32 more from three loops, and the stamp
  `INSERT`), on top of the two un-gated ledger statements. The full turso test suite dropped from ~33s to
  ~4s as a side effect of the reduced lock churn.
- **A forgotten schema change cannot silently skip existing databases.** The
  ledger's own hazard — a stamped database runs no DDL — is closed
  structurally: the fingerprint IS the gate, so a DDL change that does not
  update it leaves every stamped database re-running the DDL (loud, not
  silent), and one that does update it thereby invalidates the skip. No
  ordinary test could have caught a missed version bump: every store test
  starts from a fresh, unstamped database.
- The fingerprint covers `migrate()`'s DDL only — not `migrate_platform`'s,
  which is outside the ledger by design and runs (fail-closed) every boot.
- A libsql/SQLite upgrade that changed how stored DDL text is rendered would
  trip the fingerprint test as a false positive; the failure message says so,
  and following its instruction is harmless (the baseline is idempotent).
- **Rolling deploys where only the fingerprint moved** (no version bump): old
  and new replicas each `REPLACE` the other's ledger row, so each boot re-runs
  the 98 idempotent statements for the duration of the mixed fleet, rather than
  "once and then skips". Harmless (idempotent and concurrent-safe by the
  invariant above), and it ends when the fleet converges.
- **Multi-tenant fleets:** in the router path a tenant whose migration fails
  is `warn!`-and-skipped at platform boot and then fails loudly on lazy
  connect. No half-migrated schema is served either way, but "startup fails"
  is precise only for the single-database path — a fleet surfaces the failure
  per tenant, on first use.
- Pre-ledger databases run the baseline once more (idempotent) and are
  stamped; no migration is lost.
- The version is coarse (one baseline). Future schema changes append new
  version groups rather than growing the baseline — the constant's doc
  says so; a finer-grained per-statement ledger was considered and
  rejected as bookkeeping overhead with no added safety over the
  fail-closed baseline.

## Alternatives Considered

- **Per-statement ledger rows:** more bookkeeping, same guarantees — the
  baseline is idempotent, so statement-level tracking adds nothing until
  a non-idempotent migration exists (at which point it gets its own
  version group).
- **Keeping the swallow with logging:** a logged-but-served half-migrated
  schema is still a corrupt deployment; the failure must gate boot.
- **Version-only gating (the first cut of this ADR):** rejected — it made
  correctness depend on a human remembering to bump a constant that nothing
  checked, re-creating the documented-but-unenforced shape this issue exists
  to kill. The fingerprint gate removes the human from the correctness path.
- **Keying the ledger on the fingerprint** (`PRIMARY KEY(fingerprint)` or
  `(version, fingerprint)`) with `INSERT OR IGNORE`: rows would accumulate
  instead of replacing, which removes the rollback caveat entirely (an older
  build's row can never be destroyed) and simplifies the concurrency argument.
  It is the smaller design and is likely the right next step — but changing the
  ledger's primary key means rebuilding the table on every existing database,
  which is exactly the kind of non-idempotent migration this ADR says needs its
  own gating and serialization. Recorded as the natural follow-up rather than
  folded into this change.
- **A file-based migration system (flyway/sqlx style), where the migration
  file IS the ledger unit:** structurally forgetting-proof, but a large
  rewrite of an imperative-Rust DDL path. The fingerprint gate gives the same
  property at ~40 lines; file-based migrations remain the right long-term
  direction.
- **Serializing the migration (advisory lock / exclusive transaction):**
  unnecessary while every statement is idempotent and concurrent-safe (the
  invariant above), and it would add a cross-backend locking primitive Turso
  Cloud does not offer uniformly. Required the moment a non-idempotent
  migration exists — recorded here so that requirement is not rediscovered
  the hard way.
