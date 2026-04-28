# ADR-0064: Idempotent Spec Persistence

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0030: Hash-Gated Verification
  - ADR-0060: Bounded Warm Restart and Digest-Aware App Reconcile
  - ADR-0062: Delta OS-App Reconcile and WASM Artifacts
  - ADR-0063: Object Store for Blob Bytes
  - `crates/temper-store-turso/src/store/specs.rs`
  - `crates/temper-platform/src/bootstrap.rs`
  - `crates/temper-platform/src/os_apps/mod.rs`
  - `crates/temper-cli/src/serve/mod.rs`

## Context

After moving WASM bytes out of SQL-backed blob storage, production startup traces
showed that WASM metadata persistence was no longer the dominant cost. On the
2026-04-28 Railway deployment of OpenPaw service version
`0208e97add4a8046eb833cfc01bac1fcf42724ad`, Datadog showed
`turso.upsert_wasm_module` at 25 calls with max duration about 595 ms.

The remaining startup and boot-recovery cost was spec persistence and
verification bookkeeping. The expensive spans were almost entirely idle wait,
not CPU:

- `turso.upsert_specs_and_commit`: one startup span at about 5.36 s with about
  4.4 ms busy CPU.
- `turso.persist_spec_verification`: 25 spans, max about 19.9 s, total about
  35.1 s.
- `turso.commit_specs`: three spans, max about 12.7 s, total about 26.6 s.
- `turso.load_verification_cache`: two spans, max about 9.5 s.

The implementation was still asking Turso to participate in write transactions
for rows that were already byte-for-byte identical and already verified. This
kept warm startup proportional to the number of specs in the tenant instead of
the number of changed specs.

## Decision

### 1. Identical app spec commits are read-only

`upsert_specs_and_commit` first reads fingerprints for the incoming app specs:
content hash, CSDL, and committed status. It also checks whether the tenant
policy text and installed-app marker need writes. If nothing changed, the
function returns before acquiring the process write gate and before opening a
SQL transaction.

If some specs changed, only those specs are upserted in the transaction. The
transaction still preserves the existing semantics for real changes: changed
specs reset verification and bump version, while unchanged specs keep version
and verification state.

### 2. Verification persistence is idempotent

`persist_spec_verification` now updates a row only when the verification fields
would actually change. Replaying the same successful verification result does
not rewrite `updated_at` and does not create avoidable write contention. The
comparison intentionally ignores `verification_result.verified_at`, because an
otherwise identical verification pass should not become a durable write solely
because the verifier ran at a new wall-clock time.

### 3. Spec commit only promotes uncommitted rows

`commit_specs` now updates rows only when `committed != 1`. Calling it for a
tenant whose specs are already committed is a no-op instead of a tenant-wide
rewrite.

### 4. Verification cache only trusts committed rows

`load_verification_cache` filters to `committed = 1`. This preserves crash
safety: a row that was verified but not yet committed is not allowed to skip the
next bootstrap persistence pass.

### 5. Bootstrap persistence skips cached verified specs

System, agent, and OS-app bootstrap still parse and register specs into memory,
but durable spec persistence is limited to hashes missing from the committed,
verified cache. A warm boot with unchanged verified specs no longer runs the
`upsert_spec -> persist_spec_verification -> commit_specs` loop.

### 6. Background verification skips cached verified app specs

The `temper serve` background verifier uses the same committed verification
cache. App specs whose IOA hash is unchanged and already verified are not
flipped from `passed` to `running` and back to `passed` on warm boot. This
keeps warm boot from turning a no-op verification pass into two durable writes
per app spec.

## Consequences

### Positive

- Warm startup avoids spec write-gate contention when specs are unchanged.
- Reconcile work becomes proportional to changed specs, not installed specs.
- Crash safety is preserved because only committed specs can populate the cache.
- Datadog spans distinguish real spec changes from no-op boot bookkeeping.
- Warm `temper serve` no longer re-verifies unchanged app specs solely to
  refresh design-time verification status.

### Negative

- `upsert_specs_and_commit` performs small preflight reads before deciding
  whether a transaction is needed.
- Spec persistence has more idempotency conditions to keep covered by tests.

### Risks

- A stale verification cache could skip persistence incorrectly if it included
  uncommitted rows. This is mitigated by filtering cache reads to committed
  specs only.
- Concurrent app installs can race between the preflight read and transaction.
  The write path remains idempotent, uses unique keys, and preserves existing
  `INSERT OR IGNORE` behavior for installed-app markers.

## Rollout Plan

1. Add red tests for write-gate bypass, idempotent verification updates
   including `verified_at`-only changes, idempotent spec commits, and
   committed-only verification cache reads.
2. Make Turso spec persistence no-op before the write gate when all incoming
   app data is unchanged.
3. Pass the verified cache into bootstrap persistence so warm boots skip
   unchanged verified spec rows.
4. Make background verification skip app specs whose cached hash is still
   verified.
5. Verify locally, update OpenPaw to the new Temper revision, deploy, and
   compare Datadog startup spans for `turso.upsert_specs_and_commit`,
   `turso.persist_spec_verification`, and `turso.commit_specs`.

## Non-Goals

- Changing the entity-first Temper app architecture.
- Marking `/readyz` healthy before required startup apps are usable.
- Replacing all SQL reads in startup recovery.
- Changing WASM artifact storage; ADR-0063 owns that boundary.

## Rollback Policy

Reverting this ADR returns to the previous behavior of writing all bootstrap and
app spec rows on every persistence pass. Because the schema is unchanged, rollback
requires only code rollback.
