# ADR-0191: Host-Owned Typed Scoped Module Data

- Status: Proposed
- Date: 2026-08-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0159: Task-Scoped Schema Deployment and Migration
  - `crates/temper-spec/src/bundle/`
  - `crates/temper-runtime/src/persistence/schema_deployment.rs`
  - `crates/temper-server/src/application_data/`
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-wasm-sdk/src/data/`

## Context

ADR-0157 removes guest-selected tenant and principal identity from typed module
data, binds generated clients to an immutable application closure and exact
capability grant, and routes `DataOperationV1` through a governed host service.
That service currently dispatches every entity and File operation through the
tenant-global application-data path.

ADR-0159 adds immutable task-scoped bundles and `SchemaExecutionPin`. Scoped
actors, events, actions, reactions, OData calls, and restart recovery use the
pin as their authority for exact schema and persistence identity. Scoped bundle
module descriptors, however, record only a module name and artifact digest.
WASM dispatch still resolves the tenant-global active module and typed-data
manifest. A dynamically deployed scoped integration can therefore execute, but
its generated client cannot safely address the scoped entities that invoked it.

Adding tenant, principal, scope, or bundle fields to `DataOperationV1` would
make identity guest-selectable and recreate the spoofing boundary ADR-0157
removed. Falling back to local `/tdata` keeps HTTP headers and routing as an
ambient internal authority path. Neither is acceptable.

## Decision

### 1. Scope Is An Immutable Host Invocation Binding

WASM actor dispatch has two explicit host-owned schema targets:

```text
DispatchSchemaTarget = TenantGlobal | Scoped(SchemaExecutionPin)
```

The actor boundary constructs this target from the actor's own global identity
or immutable schema pin before WASM integration lookup. It is not inferred from
the absence of `AgentContext.schema_pin`. A scoped actor requires the complete
pin in both its actor-owned target and `AgentContext`; any missing or unequal
value fails before module, manifest, or tenant-global registry resolution.
`TenantGlobal` is admitted only when the actor is positively identified as
tenant-global and `AgentContext.schema_pin` is absent. A pin on a global actor
is also an invariant failure rather than an instruction to redirect it.

`ModuleDataTarget` is copied from the validated dispatch target before guest
execution. The invocation authority also captures the authenticated tenant and
principal, exact module name and artifact digest, and verified typed-data
manifest.

`DataOperationV1` remains unchanged and contains no tenant, principal, scope,
bundle, or grant selector. A module cannot replace or attenuate its target after
invocation begins. Scoped operations never fall back to tenant-global data or
the current active scope pointer.

**Why this approach**: the actor dispatch boundary already resolved the exact
durable schema identity. Reusing that host-owned identity closes the spoofing
surface and makes every operation consistent with the triggering actor.

### 2. Scoped Bundles Carry Exact Typed-Data Bindings

Each scoped WASM module descriptor may carry one canonical
`ModuleSdkManifest`. The descriptor records the manifest binding digest, and
that digest participates in the scoped bundle's canonical identity beside the
module name and artifact digest. The durable bundle record retains the exact
manifest required to reconstruct invocation authority after restart.

A module without a typed-data manifest may execute but receives no typed-data
host capability. A module with a manifest must satisfy all of these checks:

- the descriptor name, manifest module name, and selected integration module
  are identical;
- the descriptor artifact digest, manifest artifact digest, loaded artifact
  hash, and artifact-carried custom-section binding are identical;
- the manifest ABI, grant digest, used-symbol digest, and generator version are
  valid;
- host regeneration from the bundle's exact canonical CSDL and IOA closure is
  byte-identical, except for a valid existing additive-compatibility proof;
- the immutable scope pin names the same bundle that owns the descriptor; and
- the tenant-scoped artifact store can resolve the exact digest.

The module SDK closure digest is a domain-separated digest of canonical CSDL and
IOA generation inputs. It excludes module artifact bytes and the enclosing
bundle digest, avoiding a digest cycle because the compiled artifact itself
carries the SDK binding. The enclosing bundle digest binds that closure,
artifact, and capability manifest together.

Verification performs these checks before a bundle becomes `Verified`.
Activation repeats the immutable-record checks. Invocation repeats the cheap
identity checks and fails closed if the exact binding is absent or inconsistent.

**Why this approach**: a module artifact, generated schema, and capability grant
must be one immutable closure. Digesting only the artifact would permit schema
or grant substitution, while making the SDK depend on the final bundle digest
would create an impossible compile-time cycle.

### 3. Scoped Artifact Resolution Is Pin-Aware And Recoverable

For a scoped invocation, WASM dispatch resolves the module descriptor from the
exact `(tenant, scope, bundle digest, module name)` registry entry. It does not
use the tenant-global active module mapping. Cache misses load the exact
content-addressed artifact by digest, verify its hash, compile it, and cache it.

Registry recovery stages module descriptors and typed-data bindings from the
durable immutable `SchemaBundleRecord` together with CSDL and IOA. Recovery of a
pinned retired predecessor remains valid. The active pointer is relevant only
when creating a new entity pin; it is never used to reinterpret an invocation
that already has a pin.

**Why this approach**: module names are mutable tenant aliases. Only the
artifact digest in the exact pinned bundle is stable across activation,
replacement, retirement, process restart, and cache eviction.

### 4. One Adapter Selects Global Or Scoped Service Methods

`ApplicationDataInvocation` owns the target and selects the corresponding
governed service operation for every typed call. All schema-presence checks,
Cedar resource attributes, registry metadata, and operation routing resolve
against the same target while retaining the authenticated host principal:

- keyed reads use exact-pin hydration and state reads;
- queries enumerate only journals/index rows belonging to the exact pin and
  retain bounded fallback behavior;
- creates, patches, actions, and composites dispatch with the exact pin;
- action-derived `AgentContext` preserves the authenticated principal and pin;
- native File metadata, version, content, and write paths receive the pin; and
- batch items share the invocation target and cannot mix scopes.

The existing tenant-global methods and results remain unchanged. The adapter
does not duplicate Cedar, schema validation, guardrails, actor dispatch, query,
File, or persistence logic; it passes the immutable target to the shared
governed substrate.

**Why this approach**: one target-aware adapter preserves parity between global
and scoped behavior without adding another data service or HTTP compatibility
path.

### 5. Failure And Budget Semantics Remain Typed

Call, batch-item, page-item, request-byte, response-byte, open-response,
open-stream, and stream-byte budgets remain invocation-scoped and apply equally
to global and scoped targets. All failures after a typed request is admitted
return `DataResponseV1::error(ModuleDataError)`.

Missing pins, missing immutable bundles, module descriptor mismatch, stale
schema, closure drift, grant drift, cross-scope access, and cross-tenant access
use stable `SchemaMismatch` or `AuthorizationDenied` errors with bounded codes,
retryability, details, and Cedar decision identifiers where present. The guest
failure route preserves the complete structured error rather than reducing it
to a transport integer or message string.

Authoritative entity-valued action results and committed sequences continue to
come directly from actor responses. Scoped calls do not poll projections to
recover results.

**Why this approach**: scope selection changes routing authority, not the typed
operation contract. Equivalent failures must remain programmatically
distinguishable to generated clients.

## Rollout Plan

1. Land the immutable descriptor, closure digest, durable record, and registry
   binding with generation and verification tests.
2. Route every typed entity, query, action, composite, and File operation
   through `ModuleDataTarget`, adding fail-closed isolation tests.
3. Add persistent restart recovery and a live generated-client canary, while
   retaining the tenant-global compatibility suite.
4. Deploy the fork, exercise both scoped and global canaries, and verify
   low-cardinality binding, denial, budget, and recovery telemetry in Datadog.

## Readiness Gates

- Generated-client golden coverage uses a dynamically deployed scoped schema.
- Artifact, closure, schema, symbol, or grant drift cannot reach activation.
- A spoofed guest request cannot select another scope or tenant.
- Bounded queries and File streams cannot cross the exact schema pin.
- Persistent restart reloads the same artifact and typed binding from immutable
  storage without relying on a mutable tenant-global module alias.
- All existing global typed module-data tests remain green.
- Focused tests, full workspace tests, clippy, DST review, code review, live E2E,
  deployment, and Datadog verification pass.

## Consequences

### Positive

- Dynamically deployed schemas can use generated clients without `/tdata`.
- Guest code never receives a scope-selection primitive.
- Schema, artifact, and capability identity survive restart and retirement.
- Global and scoped operations retain one typed ABI and governed service.

### Negative

- Scoped bundle records become larger because they retain canonical typed-data
  manifests.
- Verification must regenerate SDK metadata and inspect artifact bindings.
- Query and artifact caches need exact-pin keys rather than tenant aliases.

### Risks

- Partial routing could leak global data into a scoped call. One closed target
  enum and exhaustive operation tests mitigate this.
- A digest cycle could make scoped SDKs impossible to build. The separate
  schema-closure digest deliberately excludes artifacts and the enclosing
  bundle digest.
- Restart could accidentally resolve a newer module alias. Scoped cache loading
  is content-addressed and verifies the exact descriptor digest.

### DST Compliance

- New registries and canonical inputs use `BTreeMap` and deterministic ordering.
- Digests use length-framed canonical bytes and SHA-256.
- Invocation routing uses immutable host context and performs no ambient clock,
  randomness, environment, filesystem, or network lookup.
- Persistent restart tests use simulated stores and deterministic cache
  eviction/recovery.

## Non-Goals

- Adding guest-selected tenant, principal, scope, or bundle fields.
- Replacing the public OData API.
- Allowing one invocation to mix tenant-global and scoped entities.
- Allowing one scoped invocation to address multiple scope pins.
- Adding entity-specific logic to Temper framework code.
- Preserving the scoped `/tdata` compatibility helper after downstream
  migration.

## Alternatives Considered

1. **Add scope fields to `DataOperationV1`** — rejected because guest-selected
   identity is spoofable even when Cedar later rejects most attempts.
2. **Infer the scope from entity IDs** — rejected because persistence framing is
   internal and entity identifiers are not authority.
3. **Use the current active scope pointer on every call** — rejected because an
   activation or migration could reinterpret an in-flight or recovered actor.
4. **Resolve scoped modules through the tenant-global module registry** —
   rejected because mutable aliases cannot prove the artifact in a pinned
   bundle.
5. **Keep using local `/tdata`** — rejected because it retains HTTP identity and
   routing as an internal authority path and blocks removal of the compatibility
   permit.

## Rollback Policy

Disable typed-data capability installation for scoped module descriptors while
leaving scoped actor execution and tenant-global typed module data unchanged.
Immutable bundle records and artifact bindings remain retained for audit. A
bundle that has been activated is not edited in place; correction requires a
new verified successor bundle and normal activation or migration.
