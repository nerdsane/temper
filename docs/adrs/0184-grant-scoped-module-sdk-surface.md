# ADR-0184: Grant-Scoped Module SDK Surface

- Status: Accepted
- Date: 2026-08-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0182: App-Rooted Module Binding Verification
  - `crates/temper-codegen/src/module_sdk/`
  - `crates/temper-wasm-sdk/src/data/manifest.rs`

## Context

ADR-0157 defines a module-specific least-privilege data grant and requires the
generated SDK to expose only the clients and methods in that grant. The host
already enforces the canonical artifact-bound grant independently. The Rust
generator, however, emits create, patch, filter, and order helper types for
every granted entity even when the matching operation is absent. These orphan
helper types have no corresponding generated method, but the compile-time
surface still falsely implies authority and makes small module integrations
substantially harder to review.

The generator already scopes most methods by global operation. It does not
scope File metadata reads by `metadata_read`, and its single optional-version
stream method conflates current `content_read` with `version_read`. The host
likewise enforces content/version operations on streams but currently treats a
global entity read plus a File entity entry as sufficient for metadata reads.

## Decision

Generated Rust source is a direct representation of the resolved module data
grant:

- create input types and methods require `entity_create`;
- patch input types and methods require `entity_patch`;
- filter and order helper types plus the query method require `entity_query`;
- filter and order constructors remain limited to each entity grant's declared
  fields and commit-sequence ordering flag;
- bound-action input types and methods require both the corresponding global
  action operation and the exact per-entity action entry;
- File `get` and `query` metadata methods require `metadata_read` in addition to
  the matching global entity operation;
- current-content and version-content reads are separate generated methods so
  each requires exactly `content_read` or `version_read` plus `file_read`;
- File write methods require `content_write` plus `file_write`; and
- entity values and property types remain emitted when a granted read, write,
  or action result must decode that entity.

The host continues to be the security boundary and independently validates the
artifact-bound grant for every operation. Its File metadata check is tightened
to require `metadata_read` for File `EntityGet` and `EntityQuery`, matching the
grant contract already defined by ADR-0157. Generated source reduces accidental
coupling and review noise; it does not become an authorization boundary.

`metadata_read` applies only to reading the File entity's metadata through
`get` or `query`. File create and patch remain controlled by `entity_create` and
`entity_patch`; the v1 grant has no separate metadata-write operation. A File
grant containing only content or version read can still generate its matching
stream method and client constant, but it does not expose File metadata reads.

This change does not alter the data host ABI, manifest schema, canonical grant,
or grant digest algorithm. A generated artifact's content digest naturally
changes when regenerated because its source changes. No ABI or standalone
generator-version bump is required: the existing package version continues to
identify the generator release, and deterministic tests cover source,
manifest, grant digest, and packaged artifact stability for identical inputs.

The stricter host metadata check is intentionally fail-closed. An existing
artifact that names a File entity and has `entity_get` or `entity_query` but
omits `metadata_read` will begin receiving `CapabilityDenied` rather than
reading File metadata. Such a grant did not declare the File capability defined
by ADR-0157; it must be regenerated with `metadata_read` when metadata access is
intended, or remain denied.

**Why this approach**: conditional emission fixes the abstraction at its source
without duplicating authorization state or weakening runtime checks. Keeping
the manifest and grant canonicalization unchanged preserves activation
semantics while regenerated clients intentionally lose unusable APIs.

## Rollout Plan

1. Audit checked-in application manifests and fixtures for File entity grants
   that combine `entity_get` or `entity_query` with a missing `metadata_read`.
2. Add `metadata_read` only where the module demonstrably consumes File
   metadata; leave undeclared access fail-closed.
3. Land generator and host enforcement together, regenerate representative
   clients, and prove old over-broad helpers no longer compile while intended
   least-privilege calls do.
4. Deploy the release and verify both sides live: a metadata-capable artifact
   succeeds, while an artifact without `metadata_read` receives a bounded
   authorization denial. Confirm the corresponding authorization evidence in
   Datadog without recording entity identifiers or payloads.

## Readiness Gates

- Repository grants and fixtures have no unexplained File metadata-read gaps.
- Focused generator, manifest, host-authority, generated-client compile, and
  deterministic artifact tests pass.
- The full workspace and pre-push gates pass.
- Live success and fail-closed paths are visible in deployment telemetry.

## Consequences

### Positive

- Generated clients are smaller and directly reviewable as least-privilege
  capability surfaces.
- Using an ungranted operation fails at guest compile time instead of only at
  the host boundary.
- Canonical runtime authorization remains independent and becomes exact for
  File metadata reads; binding verification remains unchanged.

### Negative

- Downstream modules that accidentally referenced ungranted helpers must add an
  explicit grant or remove the unusable call when they regenerate.
- Existing File metadata callers without `metadata_read` are denied after the
  host enforcement ships and must publish a corrected least-privilege grant.
- Golden tests must cover both positive and negative surface assertions as new
  operation families are added.

### Risks

- Over-pruning shared entity/property types could break valid action-result
  decoding. Tests therefore retain and compile the types required by granted
  reads and entity-valued actions.

## Non-Goals

- Changing Cedar authorization, dispatch, persistence, or File storage paths.
- Adding new operation or File capability kinds.
- Narrowing which entity properties may be returned by an authorized read.
- Preserving source compatibility for helper APIs that were never granted.

## Alternatives Considered

1. **Keep the broad source surface and document runtime denial** — Rejected
   because it contradicts ADR-0157 and makes generated code an inaccurate
   capability description.
2. **Generate every helper but mark ungranted APIs deprecated** — Rejected
   because deprecated APIs still compile and remain review noise.
3. **Treat generated source as the security boundary** — Rejected because
   guest code and artifacts are untrusted; the host must independently enforce
   the bound grant.

## Rollback Policy

Revert the conditional emitter changes, host File metadata check, and golden
assertions together. No stored data or manifest migration is required. Existing
artifacts then regain the prior broad metadata behavior and regenerated clients
again expose unusable helper APIs, so rollback is a temporary compatibility
measure rather than an alternate steady state.
