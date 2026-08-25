# ADR-0184: Canonical Schema Default Materialization

- Status: Proposed
- Date: 2026-08-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0179: Canonical Entity-Valued Action Results
  - ADR-0182: App-Rooted Module Binding Verification
  - ADR-0183: Restore Canonical Module-Data Bindings
  - GitHub issue #21: Application-facing typed failure envelopes
  - GitHub issue #60: Canonical typed entity responses omit defaulted non-null properties
  - `crates/temper-codegen/src/module_sdk.rs`
  - `crates/temper-wasm-sdk/src/data/manifest.rs`
  - `crates/temper-server/src/application_data/schema.rs`

## Context

Typed module-data clients deserialize keyed reads and entity-valued action results into generated Rust types. Those types correctly represent non-null CSDL properties as required fields. Runtime entity state is intentionally sparse, however, and canonical response construction currently emits only fields present in that sparse state plus selected host-owned fields. An absent non-null property is therefore omitted even when CSDL declares a `DefaultValue`, causing generated deserialization to fail after a successful, authorized read.

The installed module binding is the runtime source of truth after a workspace-free restart. Its property metadata records names, types, nullability, and enum members but not defaults, so the host cannot reconstruct the schema contract without consulting mutable workspace input. Omitting a required property with no valid default is equally unsafe: it emits a value that cannot satisfy the generated type instead of failing at the schema boundary.

Issue #21 proposes a broader application-facing failure envelope and declarative IOA routing. This decision does not introduce a competing taxonomy. It uses the existing structured module-data error envelope at the current ABI boundary; a future issue #21 adapter can preserve the stable kind and code when lifting this error into application transition routing.

## Decision

### Store validated canonical defaults in the module SDK manifest

`ManifestPropertyV1` will carry an optional canonical JSON default. SDK generation will parse each declared CSDL property or action-parameter default according to its exact scalar or enum type and reject invalid declarations. The manifest stores the typed canonical value rather than the original lexical string so runtime behavior is self-contained and cannot vary by parser or workspace availability.

Default metadata remains part of deterministic manifest serialization, the binding digest, and per-symbol semantic hashes. Consequently, changing a default changes the locked binding and compatibility calculations just as changing nullability or a property type does.

**Why this approach**: the compiled module and locked manifest must describe the exact response contract they were generated against. Re-reading CSDL at runtime would break immutable closure semantics and workspace-free restart.

### Make canonical response construction total and fallible

For every declared entity property, canonical response construction will apply this precedence:

1. use the committed field value, including canonical name matching;
2. synthesize supported host-owned values such as entity ID and status;
3. materialize the manifest's validated canonical default;
4. omit the field only when the property is nullable;
5. otherwise return a structured `SchemaMismatch` / `MissingRequiredProperty` module-data error.

Every selected or synthesized value is validated against the bound property metadata before emission. Keyed reads, query rows, create/patch results, and entity-valued action results will all use this same fallible constructor so no response path can drift.

**Why this approach**: generated types and canonical host responses become two implementations of one closed schema contract. Failing at construction preserves the actual cause and avoids presenting a guest with an undecodable success payload.

### Preserve the issue #21 boundary

The immediate error remains a bounded `ModuleDataError` with a stable kind and code. This change does not add IOA failure-routing metadata, application failure states, retry classification beyond the existing module-data contract, or message parsing. Those belong to issue #21, which can adapt this error without losing provenance.

**Why this approach**: issue #60 fixes response correctness without coupling schema materialization to the larger transition-failure architecture.

## Rollout Plan

1. Extend manifest metadata and deterministic integrity tests for canonical defaults.
2. Reject invalid property and parameter defaults during module SDK generation.
3. Route all canonical entity response paths through the fallible constructor.
4. Add generated-client and locked workspace-free restart regressions before merging.
5. Deploy the merged kernel and repeat the ARC identity scenario that originally exposed the missing `FailureReason` field.

## Readiness Gates

- Empty and non-empty strings, signed integers/counters, booleans, enums, and nullable absence are covered.
- Invalid defaults and missing required values return stable structured schema errors.
- Keyed reads and entity-valued action results decode through generated typed clients.
- Fresh install and workspace-free restart produce identical canonical values and binding digests.
- Full workspace tests, determinism review, and code-quality review pass.

## Consequences

### Positive

- Strict generated types decode successful canonical responses reliably.
- Locked bindings contain the complete response-shaping contract.
- Default changes participate in deterministic integrity and compatibility checks.
- Missing required state fails at the host schema boundary with a stable cause.

### Negative

- The manifest wire shape grows by one optional field per property or parameter with a declared default.
- Canonical response construction becomes fallible, requiring error propagation through every caller.
- Existing compiled artifacts without default metadata must be regenerated to benefit from materialization.

### Risks

- CSDL lexical forms could be normalized incorrectly. Generation-time type-specific parsing and representative tests mitigate this.
- A response path could continue bypassing the shared constructor. Call-site enumeration and keyed/query/action integration tests mitigate this.
- Default changes intentionally invalidate semantic compatibility even when the Rust field type is unchanged; this is required because observable values changed.

### DST Compliance

- Manifest properties and entities remain deterministically ordered.
- Canonical response maps are populated in manifest order and serialized deterministically by existing response handling.
- No wall-clock time, randomness, filesystem access, environment access, or concurrent task creation is introduced in simulation-visible code.

## Non-Goals

- Making required downstream schema properties nullable.
- Persisting materialized defaults into sparse entity state.
- Adding implicit type defaults when CSDL declares none.
- Implementing issue #21's application failure envelope or IOA failure routing.
- Broadening compatibility for artifacts whose locked metadata does not match their generated SDK.

## Alternatives Considered

1. **Make generated fields optional** — Rejected because it weakens valid non-null domain contracts and moves a host schema violation into every guest.
2. **Read current CSDL at response time** — Rejected because workspace-free restart and immutable binding behavior would diverge from initial install.
3. **Store the raw CSDL default string** — Rejected because runtime parsing would duplicate validation and permit parser-version-dependent behavior.
4. **Silently synthesize primitive zero values** — Rejected because absence without a declared default must fail closed, not invent domain data.
5. **Wait for issue #21** — Rejected because typed failure routing does not supply the missing schema value and is not required to preserve a structured module-data error today.

## Rollback Policy

Revert the manifest field, generation validation, and fallible constructor together, then require affected modules to regenerate against the reverted binding format. Do not retain partial runtime materialization without locked default metadata, because it would reintroduce workspace-dependent behavior.
