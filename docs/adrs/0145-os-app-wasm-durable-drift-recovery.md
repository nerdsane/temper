# ADR-0145: OS App WASM Reconcile Heals Durable Drift

## Status

Accepted

## Context

OS app reconcile stores a bundle digest in `tenant_installed_apps` and each
WASM module body in `wasm_modules`. Hot-uploaded modules are preserved across
same-bundle restarts, but a failed or partial rollout can leave those two stores
split: the installed app record says the current bundle is installed while the
durable module row is still an older `source='upload'` body.

After restart, the in-memory WASM registry is empty. Reconcile therefore must
reload bundled bytes when the durable module row does not match the bundle,
even if the app digest metadata already matches.

## Decision

OS app reconcile treats the bundled WASM phase as needed whenever either the
in-memory registry is missing the bundled hashes or durable `wasm_modules`
contains a different hash/source for any bundled module.

The replacement check for `source='upload'` modules uses durable module
comparison, not only installed app digest comparison. If the app WASM digest
changed, the bundled module replaces the upload. If the digest matches, the
bundled module replaces the upload only when the upload's `updated_at` predates
the installed app record's `last_reconciled_at` timestamp, falling back to
`installed_at` for older records.

## Consequences

- Restart recovery heals split-brain app installs where metadata is current but
  durable WASM bytes are stale.
- Same-bundle hot uploads are still preserved while the in-memory registry
  points at the uploaded hash.
- Bundle rollout no longer depends on `tenant_installed_apps.wasm_digest` being
  the only signal for WASM replacement.

## Non-Goals

- This does not add a new public upload API or change upload authorization.
- This does not remove hot-upload preservation for development workflows.

## Rollback Policy

Revert this ADR and the reconcile comparison change. Systems that have stale
uploaded WASM rows would then require a manual upload or row repair to recover.
