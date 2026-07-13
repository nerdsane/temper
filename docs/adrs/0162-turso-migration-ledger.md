# ADR-0162: Turso Schema Migration Ledger

## Status

Accepted (2026-07-12)

(Numbered 0162: 0156–0161 are claimed by concurrently open arena branches.)

## Context

`TursoEventStore::migrate()` re-ran the entire DDL script on every boot with
no record of what had been applied, and thirteen `let _ = conn.execute(...)`
sites discarded every ALTER failure. The intent was to tolerate benign
duplicate-column errors on idempotent re-runs — but the pattern equally
swallowed locked databases, disk errors, shadowed tables, and syntax errors,
so a genuinely failed migration left a half-migrated database that the
server then served against, silently (ARN-242).

## Decision

1. **Fail-closed idempotent execution.** `execute_idempotent` tolerates only
   the benign already-applied errors (duplicate column / already exists);
   everything else propagates and fails startup. All thirteen swallow sites
   and the two bespoke match blocks route through it.
2. **A durable version ledger.** `temper_schema_migrations (version, name,
   applied_at)` is created first; a successful full migration run stamps
   `SCHEMA_VERSION` (currently 1, `baseline-idempotent-ddl`). Boots where
   the ledger already shows the current version skip the DDL entirely.
3. **The contract:** EVERY change to the ledgered DDL must bump
   `SCHEMA_VERSION`, or databases stamped at the previous version will skip
   it. Platform-registry DDL lives in `router.rs::migrate_platform`, OUTSIDE
   the ledger — it runs every boot and must stay fail-closed; bumping the
   constant does nothing for it. **Invariant:** every migration statement
   must be idempotent AND safe to run concurrently with another booting
   server (two servers can both see an unstamped ledger and both run the
   DDL; today every statement is `CREATE … IF NOT EXISTS` or a
   benign-tolerated `ADD COLUMN`, and the stamp is `INSERT OR IGNORE` on the
   version primary key, so the race is harmless). A future version that
   backfills data or issues a bare `CREATE` must serialize the migration
   explicitly.
4. **Ordering:** the ledger check runs after the connection PRAGMAs (WAL,
   busy_timeout) and fully drains its query rows — an undrained statement
   before WAL was configured held a read lock that deadlocked concurrent
   writers (caught by the existing projection test during development).

## Consequences

- A real migration failure now fails boot loudly instead of leaving a
  half-migrated schema in service; operators can read the ledger to see
  what version a database is at.
- Stamped boots skip ~88 executed DDL statements (69 call sites, three of
  which are loops expanding to 22 ALTERs) — the full turso test suite dropped
  from ~33s to ~4s as a side effect of the reduced lock churn.
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
- **Serializing the migration (advisory lock / exclusive transaction):**
  unnecessary while every statement is idempotent and concurrent-safe (the
  invariant above), and it would add a cross-backend locking primitive Turso
  Cloud does not offer uniformly. Required the moment a non-idempotent
  migration exists — recorded here so that requirement is not rediscovered
  the hard way.
