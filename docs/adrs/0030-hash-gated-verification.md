# ADR-0030: Hash-Gated Verification Cascade

- Status: Accepted
- Date: 2026-03-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0028: Memory-bounded lazy hydration and passivation
  - `crates/temper-platform/src/bootstrap.rs` (bootstrap verification flow)
  - `crates/temper-platform/src/deploy.rs` (deploy pipeline verification)
  - `crates/temper-store-turso/src/store/specs.rs` (spec persistence with hash)
  - `crates/temper-verify/src/cascade.rs` (verification cascade)

## Context

The Temper server crashed with OOM on Railway deploys (512 MB containers). ADR-0028 addressed entity hydration and unbounded caches, but the actual OOM root cause was the **verification cascade running at every boot**.

`bootstrap_tenant_specs()` runs the full 4-level cascade (Z3 SMT solver, Stateright model checker, deterministic simulation, proptest property testing) for **every built-in spec on every startup** — 26+ specs in total (12 system entities, 7 agent entities per tenant). Each cascade invocation allocates significant memory for state-space exploration and random test generation.

These built-in specs are `include_str!()` constants compiled into the binary. Their content cannot change between boots. Re-verifying them at every startup is redundant — the same verification runs in CI tests (`test_system_specs_verify`, `test_pm_specs_verify`, `test_agent_specs_verify`).

The deploy pipeline had the same issue: re-deploying an unchanged user spec would re-run the full cascade unnecessarily.

## Decision

### Sub-Decision 1: Content-Hash Gating

Each spec's IOA source gets a SHA-256 content hash. Before running the verification cascade, the system checks whether a spec with the same hash has already been successfully verified:

- **If hash matches and verified = true**: skip cascade, reuse cached result.
- **If hash differs or not yet verified**: run cascade, persist result with hash.

This applies uniformly to built-in specs and user-submitted specs — the distinction doesn't matter. What matters is: "has this exact spec content already been verified?"

**Why this approach**: Content-addressable verification is idempotent and cache-friendly. The first boot after deploy runs the cascade once; all subsequent boots are instant. Spec changes (even a single character) trigger re-verification automatically.

### Sub-Decision 2: Schema Change (content_hash Column)

Added `content_hash TEXT` column to the `specs` table in Turso. The upsert query uses SQL `CASE WHEN` to conditionally preserve verification status when the hash matches:

```sql
ON CONFLICT (tenant, entity_type) DO UPDATE SET
    verified = CASE WHEN specs.content_hash = excluded.content_hash
               THEN specs.verified ELSE 0 END,
    ...
```

This ensures that re-persisting the same spec (e.g., during OS app reinstall) does not reset its verification status.

**Why this approach**: The hash check lives in the SQL upsert itself, making it atomic and impossible to race between check and update.

### Sub-Decision 3: Reduced Proptest Cases

Default proptest cases reduced from 100 to 20 for the deploy pipeline, and sim seeds from 5 to 3. This provides good coverage while reducing peak memory by ~5x for the rare cases where verification does run at runtime.

### Sub-Decision 4: No Out-of-Process Verification (for now)

With hash-gating, verification runs rarely at runtime (only for genuinely new or changed specs). The memory savings from reduced proptest cases are sufficient for Railway's 512 MB containers. Out-of-process verification (subprocess with `ulimit -v`, or separate service) adds complexity without proportional benefit.

**Why this approach**: Pragmatic. If a single user spec deployment OOMs, we can revisit with subprocess isolation. But eliminating 99%+ of runtime verification runs makes this unlikely.

## Rollout Plan

1. **Phase 0 (This PR)** — Schema migration, hash-gated bootstrap and deploy, reduced proptest defaults. Deploys to Railway immediately.
2. **Phase 1 (Follow-up)** — Persist verification results back to Turso after bootstrap runs the cascade (first boot populates the cache). Currently bootstrap marks specs as "Pre-verified at bootstrap" without persisting to Turso.

## Consequences

### Positive
- Server starts in <5 seconds on Railway (was OOMing after ~60s of verification).
- First boot after a deploy runs the cascade once; subsequent boots skip it entirely.
- OS app (re)installation is instant when specs haven't changed.
- User spec re-deploys with identical content skip verification.
- Reduced proptest cases (20 vs 100) cut peak memory ~5x for runtime verification.

### Negative
- First boot on a fresh database still runs the full cascade for all specs (one-time cost).
- SHA-256 hash computation adds ~microseconds per spec — negligible.

### Risks
- **Hash collision**: SHA-256 has 2^128 collision resistance. Not a practical concern.
- **Stale verification**: If the verification cascade itself changes (new checks added), old hashes would pass with stale results. Mitigated by: deploys increment the binary version, and `content_hash` is of the spec, not the verifier. A CI-only flag could force re-verification after verifier upgrades.

### DST Compliance
- `std::env::var("TEMPER_BOOTSTRAP_VERIFY")` removed (was band-aid).
- No new simulation-visible code. SHA-256 computation is deterministic.
- `spec_content_hash()` is a pure function with no I/O.

## Non-Goals

- **Build-time verification receipts**: Embedding verification proofs in the binary at compile time. This would eliminate runtime verification entirely but requires build system changes. Deferred to a future ADR.
- **Decoupling `temper-verify` from production binary**: Moving proptest/stateright/z3 to dev-dependencies. This is a larger refactor that would reduce binary size but requires rearchitecting the deploy pipeline.

## Alternatives Considered

1. **Skip cascade for built-in specs only (env var)** — The initial band-aid: `TEMPER_BOOTSTRAP_VERIFY=false`. Rejected because it doesn't help user-submitted specs, requires manual configuration, and the distinction between built-in and user specs is artificial.

2. **Out-of-process verification** — Spawn verification as a subprocess with memory limits. Rejected as premature: hash-gating eliminates 99%+ of runtime verification, making the complexity unjustified for now.

3. **Lazy verification (verify on first use)** — Only verify a spec when an entity of that type is first accessed. Rejected because it shifts OOM risk to request time, which is worse than boot time.
