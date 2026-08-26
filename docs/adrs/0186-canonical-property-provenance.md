# ADR-0186: Canonical Property Provenance

- Status: Proposed
- Date: 2026-08-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0179: Canonical Entity-Valued Action Results
  - ADR-0182: App-Rooted Module Binding Verification
  - ADR-0185: Canonical Schema Default Materialization
  - GitHub issue #63: Canonical typed reads default State instead of projecting runtime lifecycle
  - `crates/temper-codegen/src/module_sdk.rs`
  - `crates/temper-wasm-sdk/src/data/manifest.rs`
  - `crates/temper-server/src/application_data/schema.rs`

## Context

Canonical module-data responses combine values from three distinct authorities:
committed entity fields, host-owned entity identity, and the runtime IOA lifecycle.
The locked SDK manifest currently describes names and types but not those value
sources. Response construction therefore recognizes host-owned values by normalized
property name. A lifecycle property named `Status` receives the runtime status, while
the equally valid public name `State` is treated as an absent stored field and receives
its declared initial default. After the entity transitions, the typed response can
therefore contradict persisted runtime state.

Adding `State` as a second spelling would preserve the underlying ambiguity. Domain
schemas may contain ordinary properties with either name, and the runtime must not
guess which one represents lifecycle state. Resolving the question during each read
would also make an invalid application fail only after activation and would leave the
workspace-free binding without the decision required to shape responses.

## Decision

### Record canonical value provenance in the immutable binding

Every manifest property carries a closed `source` discriminator. Entity properties
use `StoredField`, `EntityId`, or `LifecycleStatus`; action parameters use `Input`.
The discriminator is serialized into the locked module SDK manifest and therefore
participates in the binding digest and per-symbol semantic hashes.

**Why this approach**: generated clients and the host share one immutable declaration
of where each canonical value originates. Public names remain presentation choices,
not runtime control flow.

### Resolve host-owned properties from the verified IOA and CSDL closure

SDK generation receives the verified IOA automaton associated with every granted
CSDL entity. The CSDL key identifies the `EntityId` property. The lifecycle candidate
is selected structurally from properties whose declared default equals the IOA initial
state and whose scalar or enum value domain accepts every IOA lifecycle state. Exactly
one candidate must exist. Zero or multiple candidates reject SDK generation with a
stable diagnostic naming the entity and candidates.

The lifecycle default is also validated against the IOA initial state. This does not
materialize the default into persisted fields; it only establishes the cross-schema
identity needed for canonical projection. Existing schema generation must emit that
initial default for lifecycle properties so the mapping is explicit in the verified
closure without relying on a reserved public name.

**Why this approach**: both specifications are available and verified at binding time.
The initial value is the semantic join between them, while exact cardinality makes
ambiguity an activation error instead of a production read error.

### Host-owned values take precedence

Canonical response construction switches on manifest provenance. `EntityId` and
`LifecycleStatus` are synthesized directly from `EntityState`; only `StoredField`
consults committed fields and then its validated schema default. Host-owned values
cannot be shadowed by similarly named sparse fields or by initial defaults. All values
continue through the existing bound-schema type validation.

**Why this approach**: provenance identifies the authority, so precedence no longer
depends on spelling or on whether a stale field happens to be present.

### Fail before activation and preserve immutable restart behavior

Lifecycle resolution is part of deterministic SDK generation and binding. A missing
IOA, unsupported key shape, missing lifecycle candidate, or ambiguous candidate fails
the build. The chosen provenance is embedded in the artifact binding and restored from
the content-addressed cache without consulting source workspaces.

**Why this approach**: a module that cannot receive truthful typed entities is invalid;
installing it and deferring failure until a read would violate fail-closed deployment.

## Rollout Plan

1. Add the closed provenance enum to manifest metadata and integrity tests.
2. Carry verified, entity-qualified IOA sources through local closure resolution into
   module SDK generation.
3. Resolve and validate entity ID and lifecycle sources during generation.
4. Project canonical responses exclusively by provenance.
5. Add direct generated-client and workspace-free restart regressions for a real
   `Unconfigured` to `Active` transition.

## Readiness Gates

- `State` and `Status` public names work when structurally identified.
- An ordinary non-lifecycle `State` property retains stored/default semantics.
- Zero or multiple lifecycle candidates fail generation deterministically.
- Keyed reads, queries, and entity-valued action results use the shared constructor.
- Locked restart produces identical provenance, binding digest, and canonical values.
- Workspace tests, clippy, determinism review, and code-quality review pass.

## Consequences

### Positive

- Typed responses cannot contradict runtime lifecycle because of public naming.
- Property authority is inspectable, hash-covered, and independent of workspaces.
- Future host-owned projections can extend a closed semantic contract instead of adding
  name heuristics.

### Negative

- SDK generation now requires the verified IOA side of the application closure.
- Lifecycle properties must declare the IOA initial state as their CSDL default.
- Manifest fixtures and direct codegen callers must supply explicit source metadata.

### Risks

- A domain property can coincidentally share the lifecycle initial value and accepted
  domain. Exact-cardinality rejection prevents silent selection; schema generation must
  disambiguate the model rather than relying on a name.
- Older cached bindings do not contain provenance. They must be regenerated instead of
  being interpreted with the removed name heuristic.

### DST Compliance

- Entity and property traversal remains in deterministic manifest/CSDL order.
- Candidate diagnostics are sorted before emission.
- No time, randomness, filesystem access, environment access, or concurrent work is
  added to simulation-visible runtime code.

## Non-Goals

- Reserving `State` or `Status` as a universal public property name.
- Persisting host-owned values into sparse domain fields.
- Inferring lifecycle state during runtime reads.
- Supporting composite entity keys in the module-data v1 entity identity contract.

## Alternatives Considered

1. **Recognize both `State` and `Status`** — Rejected because it remains name-based and
   becomes ambiguous when both are legitimate domain properties.
2. **Prefer a stored value over runtime lifecycle** — Rejected because stale sparse
   fields could contradict the actor's committed lifecycle.
3. **Resolve from current workspace CSDL at read time** — Rejected because immutable
   cache restarts intentionally operate without source workspaces.
4. **Fail only when constructing a response** — Rejected because invalid bindings must
   not activate successfully.

## Rollback Policy

Revert the provenance field, cross-schema resolver, and runtime projection together,
then regenerate affected artifacts. Do not retain a partial fallback to name-based
projection, because it recreates the correctness failure this decision removes.
