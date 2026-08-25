# ADR-0182: App-Rooted Module Binding Verification

- Status: Accepted
- Date: 2026-08-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0180: Local-First Immutable App Bundles
  - `crates/temper-platform/src/module_sdk_build/`
  - `crates/temper-platform/src/app_bundles/`
  - `crates/temper-platform/src/os_apps/data_binding.rs`

## Context

ADR-0157 requires publication and activation to regenerate a typed WASM
module's binding from the same locked application metadata used before
compilation. ADR-0180 introduced a source-neutral canonical bundle containing
the immutable root application and its complete dependency closure.

The canonical installer currently reconciles each application independently.
It regenerates a root module's binding from only that application's CSDL, so a
valid grant to an entity owned by a declared dependency is rejected. It also
passes the outer canonical bundle digest as both module closure identities. The
outer digest includes final artifacts, while the module metadata lock is
computed before compilation from canonical CSDL, IOA, manifests, versions, and
dependency edges. Equating those identities both breaks valid bindings and
would introduce a digest cycle if made part of the artifact.

Using the tenant's installed schema closure would make installation depend on
ambient mutable state and could incorrectly admit undeclared entity types.

## Decision

### Share one metadata-closure and lock-digest contract

The local SDK builder and canonical installer use one deterministic function
to compile an app-rooted metadata closure and construct its module-specific
lock. Inputs are the root app identity, module name, dependency-first app
order, and the exact app manifests, CSDL, and IOA sources. The function merges
and validates CSDL and IOA exactly once and computes the existing
`local-module-sdk/v1` digest.

Build-time discovery remains responsible for resolving explicit dependency
roots. Install-time discovery instead loads candidates only from the already
validated immutable materialized bundle. Both discovery paths feed the shared
compiler, so generation and verification cannot drift in ordering or digest
semantics.

**Why this approach**: the lock is a contract over metadata, not over where the
metadata was found. Sharing the contract preserves reproducibility without
coupling build-time workspace resolution to install-time cache layout.

### Verify every app as its own closure root

During canonical reconciliation, each app is verified as a root against only
the dependencies reachable from its own declared edges. For each typed module,
the installer regenerates the SDK manifest from the merged closure CSDL and the
module-specific metadata-lock digest, then compares it with the sidecar and
artifact-carried binding before loading the WASM.

Materialized app metadata must continue to match the canonical manifest. A
missing dependency, undeclared entity, metadata change, ambiguous symbol, or
binding mismatch fails installation before runtime registration.

**Why this approach**: a dependency may itself own modules with a narrower
closure than the top-level app. Treating every app as a root preserves
least-privilege availability and prevents sibling or parent schemas from
becoming accidental verification inputs.

### Keep bundle and module identities separate

The outer `CanonicalBundleManifestV1.bundle_digest` remains the content pin for
cache, transport, installation, restoration, and provenance. It is never used
as a typed module's closure or dependency-lock digest.

The module-specific metadata-lock digest remains embedded in the generated
binding. Dependency metadata changes alter that digest and cause a stale
artifact to fail verification even if its WASM bytes are otherwise intact.

**Why this approach**: the bundle digest includes compiled artifacts, whereas
the module digest must exist before compilation. Separate domain identities
avoid a digest cycle and make each mismatch diagnostically precise.

## Rollout Plan

1. Land the shared closure compiler, immutable-bundle resolver, installer
   integration, and full regression suite in one change.
2. Exercise locked local install and cache-only restart end to end before
   merge, then deploy through the normal Temper release path and verify the
   install and restoration telemetry in Datadog.

## Consequences

### Positive

- Typed modules can safely use dependency-owned entities.
- Build and install reproduce one deterministic metadata identity.
- Ambient tenant schema cannot widen a module's available surface.
- Cache restoration remains independent of source workspaces.

### Negative

- Canonical installation parses dependency metadata while verifying each
  typed module, adding bounded work proportional to the module closures.
- Changes to the resolver contract require an explicit resolver-version change
  and coordinated regeneration of bound artifacts.

### Risks

- Duplicate resolver logic could drift. The implementation mitigates this by
  sharing closure compilation and lock construction, leaving only source
  discovery path-specific.
- Incorrect app-root selection could admit sibling metadata. Regression tests
  exercise undeclared and ambient-only entities and dependency-local roots.

### DST Compliance

- The affected code is in `temper-platform`, outside simulation-visible
  crates. Closure traversal and emitted inputs nevertheless use sorted vectors
  and `BTreeMap`/`BTreeSet` so digest computation remains deterministic.

## Non-Goals

- Changing the canonical bundle digest or cache layout.
- Consulting live tenant schemas during artifact verification.
- Weakening exact artifact, grant, or compatibility-proof validation.
- Adding backward compatibility for artifacts bound to the incorrect outer
  bundle digest.

## Alternatives Considered

1. **Merge dependency CSDL into each root app** — Rejected because it duplicates
   ownership, obscures dependency boundaries, and changes source metadata.
2. **Verify against the tenant-wide schema registry** — Rejected because
   mutable ambient state could admit undeclared types and break reproducibility.
3. **Use the outer bundle digest in generated artifacts** — Rejected because
   the bundle includes those artifacts and therefore creates a digest cycle.
4. **Persist a second precomputed closure artifact in the bundle** — Rejected
   for v1 because the immutable bundle already contains all bounded inputs and
   the shared compiler can reproduce the lock without another canonical format.

## Rollback Policy

Revert the installer integration and shared resolver extraction together.
Previously generated valid dependency-aware artifacts will again fail closed;
no stored entity state or canonical cache format requires migration.
