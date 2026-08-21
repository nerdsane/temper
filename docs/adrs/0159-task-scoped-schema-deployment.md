# ADR-0159: Task-scoped schema deployment and migration

- Status: Proposed
- Date: 2026-08-15
- Deciders: Temper core maintainers
- Related:
  - ADR-0156: Immutable typed cross-entity references
  - ADR-0157: Metadata-generated typed module data SDK
  - ADR-0158: Durable and observable cross-entity reactions
  - `crates/temper-spec/`
  - `crates/temper-server/src/application_data/`
  - `crates/temper-server/src/trigger/`
  - `crates/temper-store-postgres/`
  - `crates/temper-store-turso/`
  - `crates/temper-store-sim/`

## Context

Temper currently installs one mutable CSDL and IOA registry per tenant. That is
compatible with tenant-global applications but cannot safely stage a schema for
one task, verify it without changing other tasks, migrate only that task's
entities, or atomically choose which schema a new task uses. A caller can submit
individual specs, but there is no immutable unit that binds the complete CSDL,
all IOA automata, module artifacts, policies, predecessor, verification result,
and migration program.

The missing unit creates authority and recovery ambiguity. If CSDL and IOA are
accepted separately, a crash can expose a mixed model. If activation overwrites
the registry, already-running actors and durable reactions can observe a schema
they were not created under. If migration is retried without a stable bundle
identity, the same request can mean different bytes. If source formatting or
input enumeration changes a digest, idempotency and replay cease to be useful.

ADR-0157 supplies the shared governed application-data service and stable
transport-neutral request/error conventions. ADR-0158 supplies durable intent,
receipt, lease, fencing, bounded supervisor, and crash-recovery patterns.
ADR-0156 defines the typed-reference and hot-reload contracts required to pin
actors and reactions to immutable schema versions. This decision composes those
foundations without creating a second data path or weakening tenant-global
compatibility.

## Decision

### 1. The immutable bundle is the authority boundary

Introduce `ScopedSpecBundle`, an immutable bundle for one tenant-local scope.
The v1 scope kind is `task`; the identifier is an opaque non-empty UTF-8 string.
A bundle contains:

- contract version;
- scope kind and scope identifier;
- optional predecessor bundle digest;
- one CSDL document;
- one IOA automaton per fully qualified entity type;
- deterministically ordered Cedar policy artifacts;
- deterministically ordered WASM module descriptors and SHA-256 digests;
- an optional migration module descriptor; and
- explicit verification and execution budgets.

The IOA declaration remains authoritative for behavior. Canonical CSDL is the
data/API projection. A bundle is rejected when two inputs declare the same
fully qualified entity type, when an input key disagrees with its automaton
name, or when the CSDL contains duplicate namespaces or duplicate named members
within a namespace. No duplicate is merged with last-writer-wins semantics.

The bundle compiler is pure: it accepts bytes and bounded metadata, parses all
IOA and CSDL inputs, returns canonical bytes plus a digest, and performs no
filesystem, network, registry, clock, randomness, or database access.

**Why this approach**: one immutable authority unit prevents mixed-schema
visibility and gives verification, authorization, storage, migration, runtime
pinning, audit, and replay the same identity.

### 2. Canonical bytes define identity

Canonicalization is versioned as `scoped-spec-bundle/v1`.

- IOA TOML is parsed into the typed `Automaton` model and serialized through a
  stable canonical representation. Insignificant whitespace and comments do
  not affect identity. Declaration arrays retain order because action, trigger,
  effect, key, and parameter order can carry meaning.
- CSDL is parsed, all semantically unordered named collections are sorted by
  canonical name, and the existing emitter produces UTF-8 XML with fixed
  indentation and line endings. Ordered collections such as key properties,
  action parameters, navigation constraints, and annotation collections retain
  order. Record annotation properties are emitted in lexical key order.
- Policy and module entries are sorted by stable logical name. Each content
  section is length-framed; concatenation is never ambiguous.
- The SHA-256 digest covers the contract version, scope kind, scope identifier,
  predecessor (or an explicit absence marker), canonical CSDL, every fully
  qualified IOA name and canonical source, policy bytes, module descriptors,
  migration descriptor, and budgets.
- Digests use lowercase `sha256:<64 hex>` form.

Recompiling a canonical bundle is idempotent. Parsing and emitting its CSDL and
IOA sources preserves the same semantic model and digest. A future canonical
format uses a new contract version; it never silently changes v1 identity.

**Why this approach**: deterministic content identity makes submit retries,
verification receipts, migration replay, and backend comparisons exact while
still ignoring irrelevant author formatting.

### 3. Deployment lifecycle is explicit and monotonic

A deployment record moves through:

`Submitted -> Verifying -> Verified -> Activating -> Active -> Retiring -> Retired`

Failures from `Verifying` or `Activating` move to `Rejected`. A rejected digest
is immutable; correction creates a new digest. A transient worker failure does
not change lifecycle state: the fenced worker lease expires and another worker
resumes from the durable cursor.

Only one bundle is active for `(tenant, scope kind, scope id)`. Activation uses
one compare-and-swap transaction over the active pointer, expected predecessor,
deployment fence, and verification receipt. Readers observe the complete old
bundle or complete new bundle, never a partially replaced collection. The old
bundle remains addressable while pinned entities, actors, events, reactions, or
receipts can refer to it.

Retirement prevents new pins but does not delete immutable artifacts. Physical
collection is a separate retention decision and requires proof that no durable
pin remains.

### 4. API and WASM calls share one governed deployment service

HTTP handlers and typed WASM host calls adapt into one
`GovernedSchemaDeploymentService`; neither path calls stores or the registry
directly. Invocation-bound tenant and principal identity follow ADR-0157: WASM
guests cannot supply bearer tokens, tenant headers, or a replacement principal.

The v1 operations are:

- `schema_bundle_submit`
- `schema_bundle_get`
- `schema_bundle_verify`
- `schema_bundle_activate`
- `schema_bundle_retire`
- `schema_migration_start`
- `schema_migration_get`
- `schema_migration_retry`

External routes are versioned under `/api/v1/schema-deployments`; typed WASM
requests use the same transport-neutral structs and stable error codes. Every
mutating request carries an idempotency key, expected scope, expected digest,
and where applicable expected predecessor/fence. Reusing a key with identical
canonical input returns the original receipt. Reusing it with different input
returns `idempotency_conflict` and performs no write.

Cedar authorizes the exact operation against a tenant-and-scope resource before
the service reads private artifacts or mutates lifecycle state. Read authority
does not imply submit, verify, activate, retire, or migrate authority. Migration
execution repeats authorization under the principal captured by the accepted
request; an operator retry cannot replace it.

Responses return redacted receipts containing request ID, scope, bundle digest,
lifecycle state, fence, predecessor, verification/migration receipt IDs, and
committed sequence. They never expose private principal claims, policy source,
WASM bytes, or migration entity payloads.

Stable v1 errors include `invalid_bundle`, `duplicate_symbol`,
`scope_mismatch`, `digest_mismatch`, `predecessor_mismatch`,
`idempotency_conflict`, `verification_failed`, `authorization_denied`,
`invalid_lifecycle_transition`, `stale_fence`, `migration_budget_exhausted`,
`migration_rejected`, `migration_failed`, and `backend_unavailable`.

### 5. Durable records mirror the state machine

Postgres, Turso, and Sim implement the same storage contract for:

- immutable canonical bundle records and artifact blobs;
- deployment lifecycle journals;
- active scope pointers;
- idempotency request records;
- verification receipts with verifier/ABI versions and input digests;
- migration jobs, leases, monotonic fences, cursors, batch receipts, and
  terminal receipts; and
- durable schema pins referenced by entities, actor snapshots, events, reaction
  intents, and reaction receipts.

Submitting a bundle and recording its idempotency mapping is atomic. Activating
a bundle and replacing the active pointer is atomic. A migration batch commits
its transformed target rows, cursor, consumed budgets, and batch receipt in one
transaction. Backends expose bounded keyset reads; none may recover by loading
an unbounded scope into memory. Redis may wake workers or coordinate placement,
but it is not authoritative for deployment or migration state.

Contract tests run the same scenarios against all three stores. The preparatory
implementation supplies fixtures and the test matrix without installing a
second temporary store API; task 114 adds the trait alongside its first durable
implementation so the contract reflects the real transaction boundary.

### 6. Migration is a pure, deterministic WASM transform

The migration module exports one versioned function conceptually equivalent to:

```text
migrate_v1(input: MigrationInputV1) -> MigrationOutputV1
```

Input contains old/new bundle digests, fully qualified entity type, entity ID,
old committed sequence, canonical old state JSON, and a deterministic logical
context. Output is `unchanged`, `replace(canonical state JSON)`, or
`reject(stable code, bounded message)`. It cannot create arbitrary cross-entity
writes; declared fan-out requires a future ABI.

The sandbox exposes no WASI filesystem, network, wall clock, randomness,
environment, host HTTP, application-data calls, actor dispatch, or registry
mutation. Fuel, memory, input/output bytes, entities per batch, total entities,
attempts, and logical duration are explicit positive budgets. JSON object keys
are canonicalized and floating-point non-finite values are forbidden.

The kernel runs the transform at least twice for sanitized verification vectors
and requires byte-identical output. Replay of a committed batch is recognized
by `(job, source cursor, input digest, output digest)` and cannot apply a second
effect. A mismatch is `migration_rejected`, not a best-effort warning.

### 7. Migration uses shadow state and atomic cutover

Migration snapshots the old active digest and fence, scans source entities by a
stable keyset cursor, and writes transformed rows into storage keyed by the new
bundle digest. New writes remain on the old bundle until cutover. A bounded
catch-up cursor replays old-bundle events into shadow state under the same pure
transform.

Cutover requires: verified target receipt, expected predecessor, current source
fence, completed scan, caught-up event cursor, validation receipt, and no
unresolved batch. One transaction advances the active pointer and fence. After
commit, recovery is forward-only: the new bundle stays active and workers
finish post-cutover bookkeeping. Before commit, recovery resumes or abandons
shadow state without changing readers.

Task 58 owns typed-reference validation, actor/reaction bundle pinning, actor
eviction/rehydration, and hot-reload audit. Task 114 connects those contracts to
this lifecycle. Until that dependency lands, task 117 must not mutate the live
`SpecRegistry` or infer pins from current registry contents.

### 8. Tenant-global behavior remains compatible

Existing tenant-global registry lookup and OData metadata remain unchanged when
no task scope is present. Scoped lookup is explicit and cannot fall back from a
malformed or unauthorized scope to tenant-global state. A valid scope with no
active bundle may use tenant-global behavior only when its creation record
explicitly selected that compatibility mode.

Existing entities retain their recorded global or scoped bundle identity.
There is no implicit adoption of the newest active bundle and no in-place
reinterpretation of durable state.

The canonical scoped-journal entity-ID frame is an internal reserved form.
Tenant-global IDs continue to support colons, but the global actor boundary
rejects any ID that exactly parses as that frame. This narrow reservation keeps
global persistence IDs compatible while making global and scoped actor and
journal identities disjoint.

### 9. Durable entity pins are authoritative during dispatch and recovery

A scoped entity operation resolves exactly one `SchemaExecutionPin` before it
selects an actor or transition table. The resolved pin is the common input for
the actor-registry key, durable persistence identity, event/action pin, exact
bundle configuration, and `TransitionTable`. Those identities must agree; the
runtime must not independently derive any of them from the current registry
contents after resolution.

The active scope pointer authorizes new entity pins and identifies the committed
side of a migration cutover. It does not reinterpret an entity that already has
a durable pin. For entity-addressed reads, writes, and bound actions, recovery
must inspect the durable entity identity before spawning an actor. A request may
name an exact bundle digest. That digest must agree with both the authoritative
active/cutover side and the entity's durable pin. A scope-only request uses the
authoritative durable pin for an existing entity and the active pointer only for
a new entity.

Any missing, malformed, ambiguous, or inconsistent identity fails closed with a
stable `schema_pin_mismatch` error (`SchemaPinMismatch` on OData). In particular,
the server must not handle a mismatch by spawning a fresh actor in the bundle's
initial state, by retrying under the active pointer, or by falling back to the
tenant-global table. Exact artifacts for a retired bundle remain available for
existing pins and recovery, but retirement prevents creation of a new pin.

Migration preserves old-or-new visibility. Before cutover, the source entity pin
is authoritative even if target shadow state exists. After the atomic cutover,
the migrated target pin is authoritative. Passivation, process restart, active
pointer recovery, and backend selection cannot change which side is visible.
Tests must exercise transition followed by another action both before and after
restart; checking only the projected target state is insufficient because a
wrong table can replay the state while rejecting the next valid action.

## Rollout Plan

1. **Deterministic foundation** — Land this ADR, pure bundle compiler,
   canonicalization/digest fixtures, duplicate checks, and sanitized handoff
   vectors. Do not wire the active registry.
2. **Task-58 integration** — After immutable typed references, actor/reaction
   pins, and hot-reload audit land, implement the governed service, durable
   backend contract, lifecycle workers, and scoped lookup in the same feature
   PR.
3. **Migration and cutover** — Enable sandboxed shadow migration only after
   backend crash matrices and old-or-new visibility tests pass for Postgres,
   Turso, and Sim.
4. **Compatibility canary** — Exercise tenant-global and opt-in task-scoped
   tenants together, compare receipts and Datadog traces, then expand rollout.
5. **Production readiness** — Merge only after full workspace, clippy, DST,
   code review, local live E2E, fork deployment, and Datadog verification.

## Readiness Gates

- Canonical bundle fixtures produce byte-identical CSDL, IOA, and digests across
  input order, formatting, repeated parsing, and process restart.
- Duplicate symbols, scope escape, stale predecessor, conflicting idempotency,
  stale fence, forbidden imports, nondeterministic output, and every exhausted
  budget fail with stable codes and no partial state.
- Postgres, Turso, and Sim pass one shared lifecycle/migration contract suite.
- Crash injection passes before and after bundle insert, idempotency insert,
  verification receipt, lease claim, each migration batch, validation, pointer
  compare-and-swap, and cutover acknowledgement.
- Concurrent readers observe only the old or new digest at every crash point.
- Existing tenant-global API, WASM SDK, actor, reaction, and metadata tests stay
  green.
- Live local and deployed canaries show scoped and global behavior with bounded,
  low-cardinality telemetry and no private payloads.

## Consequences

### Positive

- Task-specific schemas become independently verifiable, auditable, retryable,
  and atomically activatable.
- One digest ties API requests, Cedar decisions, store records, actors,
  reactions, migrations, and telemetry to identical bytes.
- The pure compiler can be heavily tested before mutable runtime integration.
- Tenant-global applications retain their current behavior.

### Negative

- Immutable artifacts and pins increase durable metadata and retention work.
- Activation and migration require explicit fences, cursors, receipts, and more
  backend transactions.
- Canonicalization is a public compatibility contract that must be versioned
  rather than casually changed.

### Risks

- An incomplete canonical form could alias distinct behavior or split identical
  behavior. Length framing, typed parsing, golden vectors, and versioning
  mitigate this.
- Missing pins could let old work execute under new behavior. Activation remains
  disabled until task 58's pinning contracts are integrated and tested.
- Migration modules process private state. The minimal pure ABI, forbidden host
  imports, redacted receipts, and bounded vectors reduce exposure.
- Backend parity may hide a weaker transaction boundary. Shared contract and
  crash tests are required before enabling a backend.

### DST Compliance

- The compiler uses ordered collections, typed parsing, fixed canonical output,
  SHA-256, and no ambient I/O, time, or randomness.
- Lifecycle workers use simulation clocks, keyset cursors, bounded batches,
  deterministic backoff, monotonic fences, and durable wakeups following
  ADR-0158's supervisor pattern.
- Migration replay is driven only by durable input, explicit budgets, and
  canonical bytes. Sim tests enumerate crash points and concurrent schedules.

## Non-Goals

- Replacing IOA as behavior authority or CSDL as the OData projection.
- Allowing per-task schema to escape its tenant or access another scope.
- Implementing task-58-owned typed references, pins, actor lifecycle, or
  hot-reload audit in the preparatory phase.
- Installing a second HTTP, OData, or WASM data path.
- Supporting nondeterministic, networked, filesystem, or cross-entity migration
  modules in v1.
- Deleting immutable bundles while durable pins may remain.

## Alternatives Considered

1. **Mutate the tenant registry in place** — rejected because concurrent tasks,
   actors, and reactions could observe mixed or retroactive behavior.
2. **Store CSDL and IOA as independent revisions** — rejected because partial
   activation and ambiguous recovery remain possible.
3. **Hash raw uploaded bytes** — rejected because whitespace, comments, and
   input enumeration would make semantic identity unstable.
4. **Use CSDL as the behavioral source** — rejected because IOA owns transition
   semantics and Cedar/reaction integration.
5. **Run migrations in an administrator client** — rejected because authority,
   receipts, budgets, replay, and state transitions would leave Temper.
6. **Let migration WASM call application data** — rejected because hidden
   fan-out prevents deterministic replay and atomic batch accounting.
7. **Wire a temporary scoped registry before task 58** — rejected because it
   would invent incompatible pinning and hot-reload behavior that task 114 must
   later remove.

## Rollback Policy

Before any scoped activation, disable submission and retain immutable records
for audit. After activation, rollback is a new forward activation whose target
is a previously verified compatible bundle; the active pointer is never edited
out of band. After migration cutover, recovery remains forward-only. Reversing
data shape requires an explicit verified migration bundle and the same fenced
cutover protocol.
