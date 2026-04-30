# ADR-0075: Tenant Secrets Key Management

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0044: Platform Secrets Untangle Default Tenant
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - `crates/temper-server/src/secrets/`
  - `crates/temper-store-postgres/src/platform.rs`

## Context

Postgres parity includes tenant secret storage. Moving secrets from Turso to Postgres must not weaken the existing "encrypted before persistence, filtered before WASM injection" contract.

## Decision

Tenant secrets are encrypted in application code before database persistence. Postgres stores only:

- `tenant`
- `key_name`
- AES-256-GCM ciphertext bytes
- nonce bytes
- timestamps

The master key is provided through `TEMPER_VAULT_KEY` as base64-encoded 32 bytes in production. If absent, local development may generate an ephemeral in-memory key, but that mode is not valid for production because persisted secrets cannot be decrypted across restarts.

Secret injection into WASM remains filtered by authorization before constructing the guest environment. Storage migration does not change the Cedar surface.

## Readiness Gates

- Production has `TEMPER_VAULT_KEY` set before any Postgres cutover that migrates secrets.
- A staging cutover verifies one migrated secret can be decrypted after service restart.
- WASM secret filtering tests pass for allowed and denied modules.
- Migration manifests count tenant secrets without logging plaintext.

## Consequences

### Positive

- Database administrators and backups do not receive plaintext secrets.
- The same vault boundary works for Turso and Postgres-backed storage.

### Negative

- Losing `TEMPER_VAULT_KEY` makes persisted secrets unrecoverable.
- Rotating the key requires a deliberate decrypt/re-encrypt migration.

### DST Compliance

Secret encryption uses production randomness and is outside deterministic simulation. Sim tests should use fixed fixture values or mocked vault behavior.

## Rollback Policy

Before cutover, continue serving secrets from Turso. After cutover, rollback requires the same `TEMPER_VAULT_KEY` to remain available so the Postgres target can be inspected or re-migrated safely.
