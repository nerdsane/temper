# ADR-0044: Platform secrets layer and default-tenant untangling

- Status: Accepted
- Date: 2026-04-15
- Deciders: Temper core maintainers
- Related:
  - ADR-0042: Crucible reference app — Environments API with full config surface
  - `crates/temper-server/src/secrets/vault.rs`
  - `crates/temper-cli/src/serve/mod.rs`
  - `crates/temper-platform/src/tenant_access.rs`
  - `crates/temper-store-turso/src/router.rs`

## Context

Temper accumulated two different tenant problems over time:

1. The string `"default"` drifted from being a normal fallback tenant name into a privileged control-plane escape hatch. It simultaneously acted as the baseline secrets bucket, an authorization bypass in tenant access middleware, and a routing shortcut to the shared platform database.
2. The personal tenant name `"rita-agents"` leaked into docs, skills, and tests, which makes the repository look preconfigured for one operator instead of a reusable platform.

Those behaviors are unrelated and should not be coupled to one tenant label. The platform already has a real system tenant, `"temper-system"`, for infrastructure-owned behavior. Everything else should behave like an ordinary tenant, including `"default"`.

## Decision

Temper will split shared infrastructure secrets from tenant-scoped secrets, remove `"default"` privilege checks, and purge `"rita-agents"` from repository-facing guidance.

### Sub-Decision 1: Add a platform secrets layer

`SecretsVault` gets an explicit in-memory `platform` bucket that sits alongside the per-tenant cache. Shared infrastructure values such as API keys and service URLs are cached there, and tenant secret reads fall back to that bucket.

`get_secret(tenant, key)` now means:

1. read the tenant override if present
2. otherwise read the platform baseline

`get_tenant_secrets(tenant)` and key listing follow the same model so the WASM host and admin APIs see the effective merged view.

**Why this approach**: the old `cache["default"]` fallback was already a structural baseline, just hidden behind a tenant label. Making it explicit preserves the behavior we actually wanted while removing the accidental tenant semantics.

### Sub-Decision 2: Only `temper-system` bypasses tenant access checks

The tenant access middleware in `temper-platform` now treats only `"temper-system"` as always accessible. `"default"` no longer bypasses the router-backed access check.

**Why this approach**: authorization bypass belongs to the system tenant, not to whichever tenant name happens to be the local default.

### Sub-Decision 3: Only `temper-system` routes to the platform database

`TenantStoreRouter::store_for_tenant` now shares the platform database only for `"temper-system"`. Any other tenant, including `"default"`, must have its own provisioned store or the call fails.

**Why this approach**: database routing is infrastructure topology. It should be explicit and auditable rather than inferred from a human-facing default name.

### Sub-Decision 4: Purge `rita-agents` from repository guidance

Repository docs, skills, and tests now use placeholders such as `{tenant}`, `"my-tenant"`, or `"test-tenant"` instead of `"rita-agents"`.

**Why this approach**: the repository should teach configuration, not smuggle in one operator's local tenant name.

## Rollout Plan

1. **Phase 0 (this PR)** — add the platform secrets layer to `SecretsVault`, move CLI seed secrets to the platform cache, remove `"default"` privilege checks, and clean up repository references to `"rita-agents"`.
2. **Phase 1 (consumer follow-up)** — update downstream apps such as OpenPaw to seed their startup secrets into the platform cache instead of dual-writing to `"default"`.
3. **Phase 2 (operational cleanup)** — rotate any local/global instructions that still mention `"rita-agents"` outside the repository.

## Consequences

### Positive
- `"default"` is once again just a tenant name, not a backdoor.
- Shared bootstrap configuration has a first-class home in `SecretsVault`.
- Tenant routing and authorization now line up with the existing `"temper-system"` system boundary.
- New users no longer encounter a leaked personal tenant name in repository instructions.

### Negative
- Any code that assumed `"default"` was always routable or always authorized must now either provision a real tenant or use `"temper-system"` intentionally.
- The vault now has one more cache surface to reason about when debugging effective secrets.

### Risks
- Downstream applications that still dual-write into `"default"` may temporarily rely on old behavior until they adopt the new platform cache. Mitigation: the fallback shape is unchanged at read time, so the migration is mostly mechanical.
- Operators may expect persisted `"default"` secrets to become platform secrets automatically. Mitigation: this ADR intentionally scopes the change to the in-memory vault and CLI bootstrap path; persistence migrations stay in app-specific follow-ups.

### DST Compliance

- `SecretsVault` lives in `temper-server`, a simulation-visible crate, but the new platform bucket is just another `RwLock<BTreeMap<...>>` alongside the existing tenant cache.
- No new nondeterministic behavior is introduced. The vault remains behind I/O traits and is not invoked from deterministic simulation paths.
- Existing `// determinism-ok` annotations remain sufficient because the only cryptographic behavior is unchanged AES-GCM nonce generation and encryption/decryption.

## Non-Goals

- Changing `TenantId::default()` away from `"default"`.
- Changing SDK or REPL defaults that still use `"default"` as a convenient tenant name.
- Adding a persisted platform-secrets table or cross-tenant migration layer in Temper itself.
- Redesigning multi-tenant provisioning UX.

## Alternatives Considered

1. **Keep using `"default"` as the hidden platform bucket** — Rejected because it preserves the ambiguity that caused the access and routing shortcuts to accrete around that tenant.
2. **Make all tenants privileged in single-user mode** — Rejected because authorization and routing semantics should not depend on deployment scale.
3. **Persist platform secrets in a new backend table right away** — Rejected for this PR because the existing runtime-only bootstrap path already solves the leaking-tenant problem, and persistence migration belongs with downstream apps that currently store bootstrap secrets under tenant IDs.

## Rollback Policy

Revert this PR. The platform secrets fallback is structurally identical to the old `"default"` fallback, so rollback is a straight code reversal rather than a data migration.
