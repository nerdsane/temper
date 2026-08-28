# ADR-0192: Governed Schema Bootstrap Dispatch

- Status: Proposed
- Date: 2026-08-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0159: Task-Scoped Schema Deployment and Migration
  - ADR-0191: Host-Owned Typed Scoped Module Data
  - `crates/temper-runtime/src/persistence/schema_deployment.rs`
  - `crates/temper-server/src/schema_deployment/`
  - `crates/temper-server/src/application_data/`
  - `crates/temper-wasm-sdk/src/schema_deployment.rs`

## Context

ADR-0191 lets a WASM module already executing from a scoped actor use typed
application data under that actor's immutable `SchemaExecutionPin`. It
deliberately rejects a tenant-global actor that merely carries a scoped pin,
and `DataOperationV1` deliberately has no tenant, scope, bundle, or principal
selector.

A tenant-global schema-deployment workflow can submit, verify, and activate a
scoped bundle, but no scoped actor exists yet to initiate the first operation.
Using `/tdata`, request headers, or a mutable module alias to create that first
actor would reopen the authority boundary ADR-0191 closed. Copying a scoped pin
onto the tenant-global actor would also confuse invocation authority with actor
identity.

The first creation and optional action must be safe to retry after cache
eviction, process restart, or an ambiguous response. The actor cache cannot
provide this guarantee because it expires and cannot reproduce the exact
original receipt. Creation and action also commit in separate durable
subsystems, so treating them as one atomic transaction would report outcomes
that the persistence history cannot support.

## Decision

### 1. Bootstrap Is A Schema-Deployment Operation

Add `SchemaDeploymentOperationV1::BootstrapDispatch`. Do not add a
scope-selecting `DataOperationV1` variant. The request contains an idempotency
key, entity type, entity identifier, initial fields, and an optional initial
action with parameters. It contains no tenant, principal, scope, bundle digest,
module alias, grant selector, or schema pin.

The host invocation resolves tenant and caller authority from the authenticated
WASM invocation. The schema-deployment service resolves `(scope,
bundle_digest)` from the durable active deployment pointer and constructs the
exact `SchemaExecutionPin`. The pointer must still name the same verified,
active bundle when the operation reservation is created. A stale, retired,
predecessor, mismatched, or unverified bundle fails closed.

**Why this approach**: schema deployment owns activation and is the only
authority that can turn a mutable active pointer into a new immutable pin.
Keeping bootstrap there prevents the ordinary data ABI from becoming a scope
selection surface.

### 2. Bootstrap Has A Dedicated Capability And Cedar Action

Add `DataOperationKind::SchemaBootstrapDispatch` and require that exact
artifact grant in the verified module SDK manifest. Bootstrap authorization
uses a distinct Cedar action and resource attributes describing the resolved
tenant, caller authority, scope, bundle digest, entity type, and requested
action. Existing create or action grants do not imply bootstrap authority, and
bootstrap does not imply ordinary typed data access.

The authenticated artifact identity, manifest binding, and caller authority
must match the host-owned schema-deployment invocation. A lookalike module name
or artifact cannot inherit another artifact's grant.

**Why this approach**: bootstrapping crosses from a tenant-global workflow into
a newly activated scoped schema. That is a stronger authority boundary than an
operation originating from an existing scoped actor and must be independently
auditable and revocable.

### 3. Validation Uses The Exact Canonical Bundle Closure

Before creating or dispatching, validate the entity type, entity identifier,
initial fields, optional action, and parameters against the exact bundle's
canonical CSDL and IOA closure. Validation must not consult tenant-global
schema, a mutable registry alias, or a newer active bundle after reservation.

The service then calls `get_or_create_scoped_entity` with the resolved pin and
dispatches the optional action with an `AgentContext` carrying that same pin.
The tenant-global actor remains tenant-global; the bridge creates and addresses
the normal scoped actor instead of attaching scoped authority to the caller.

Each call targets one entity. A new reservation requires that entity to be
absent. Replaying the same reserved operation may observe the entity and resume
from its journal identity. No collection-wide "scope is empty" check is made.

**Why this approach**: exact-closure validation and normal scoped actor routing
preserve schema, Cedar, guard, persistence, recovery, and deterministic actor
semantics without introducing a second entity implementation.

### 4. A Durable Coordinator Owns Idempotency

The schema-deployment store retains a bootstrap operation record keyed by
`(tenant, caller_authority, idempotency_key)`. Its reservation includes a
domain-separated canonical request digest, exact pin, entity journal identity,
derived creation idempotency identity, optional action idempotency key, and
bounded progress state. A second durable ownership claim keyed by `(tenant,
pin, entity_type, entity_id)` names the one bootstrap operation allowed to
create or recover that target.

Reservation atomically:

1. confirms the exact bundle is verified and is the current active pointer;
2. inserts the operation and acquires the target ownership claim in the same
   schema-deployment-store transaction if both keys are new;
3. returns the existing operation only when the caller and canonical request
   digest match exactly and that operation still owns the target; or
4. rejects a different operation that already owns or attempts to claim the
   same target.

Reusing a key with a different request is a conflict. A different key or caller
targeting the same entity is also a creation conflict, even if its request bytes
are otherwise identical. After reservation, the coordinator performs normal
scoped creation, records the authoritative creation sequence, performs the
optional action, and persists the final receipt. Store updates use
compare-and-set progress so concurrent retries converge on one record.

On recovery, an incomplete reservation resumes only after confirming its target
ownership claim. The deterministic journal identity makes an already committed
creation observable as the same entity for that owning operation, and the
action's durable idempotency key prevents duplicate dispatch. An unowned
pre-existing journal is a conflict, never a replay. The authoritative persisted
receipt, not a reconstructed approximation, is returned for every completed
replay after cache eviction or cold restart.

This is not a cross-store transaction. If creation commits and the optional
action rejects, times out, or exhausts a budget, the entity remains committed
at its creation sequence and the receipt records that partial outcome. An
operation is complete once its authoritative success or bounded failure receipt
has been persisted.

**Why this approach**: durable operation state closes ambiguous-response and
restart windows while respecting the independent commit points already present
in entity journals and action dispatch.

### 5. The Receipt Is Bootstrap-Specific And Bounded

Return a bootstrap receipt containing:

- the exact `SchemaExecutionPin`;
- entity type and identifier;
- the authoritative creation sequence;
- optional action sequence and typed result when committed; and
- a bounded structured failure classification, code, retryability, decision
  identifier, and details when creation or action does not complete normally.

The receipt distinguishes reservation/validation/authorization failure,
creation conflict or failure, and post-creation action failure. It never hides
a committed creation behind an action error and never relies on projection
polling to discover committed sequences or results.

Request, response, entity, field, parameter, and failure-detail budgets are
explicit and enforced before allocation or persistence. Persisted receipts are
bounded by the same contract returned through the SDK.

**Why this approach**: callers need an authoritative replayable outcome that
matches durable history, including partial outcomes, without interpreting
transport errors or polling eventually consistent projections.

## Rollout Plan

1. Add the SDK operation, dedicated grant kind, Cedar action, bounded request,
   receipt, and error contracts.
2. Add the schema-deployment bootstrap record and atomic reserve/progress/
   receipt store contract to simulated, PostgreSQL, and Turso stores.
3. Add exact-bundle validation, scoped creation/action dispatch, and recovery
   coordination in the schema-deployment service.
4. Add grant, Cedar, closure, idempotency, partial-outcome, cache-eviction, and
   cold-restart E2E coverage while retaining global and scoped typed-data tests.
5. Run mandatory reviews and validation, deploy, exercise a live bootstrap, and
   verify operation and denial telemetry in Datadog.

## Readiness Gates

- A tenant-global deployment workflow bootstraps the first scoped entity with
  no `/tdata` call or scope-bearing header.
- The entity and optional action use the exact active pin reserved by the
  coordinator and survive persistent restart.
- Same-key retries return the byte-equivalent authoritative receipt; conflicting
  requests fail without dispatch.
- Concurrent different-key or different-caller attempts for one exact target
  admit one durable owner; every non-owner receives a creation conflict and
  cannot adopt the winner's journal as replay.
- Grant, Cedar, tenant, scope, bundle lifecycle, canonical closure, budget, and
  lookalike-artifact failures all fail closed with bounded classifications.
- Injected failures across every reservation, creation, action, and receipt
  persistence boundary converge without duplicate creation or action dispatch.
- Existing tenant-global and scoped typed module-data behavior is unchanged.

## Consequences

### Positive

- A newly activated scoped schema can be entered through a governed host-owned
  bridge without weakening typed data authority.
- Retries and cold recovery return the exact original outcome.
- Partial creation/action outcomes match the durable entity and action history.
- Bootstrap authority is separately grantable, authorizable, observable, and
  revocable.

### Negative

- Schema-deployment stores gain another durable record and compare-and-set
  workflow.
- Operations can remain incomplete across crashes and require recovery work.
- Callers must handle an honest partial outcome where creation committed but
  the optional action did not.

### Risks

- A recovery race could dispatch the action twice. A durable action
  idempotency key and store progress compare-and-set make retries converge.
- Two independently idempotent operations could race for one entity journal.
  The atomic target ownership claim admits only one operation and recovery
  verifies ownership before observing an existing journal.
- Mutable-pointer drift could bind a retry to a newer bundle. The reserved pin
  and request digest remain immutable; only the first reservation consults the
  active pointer.
- Unbounded persisted failures could amplify storage. SDK and store contracts
  enforce the same explicit byte and item budgets.
- Incomplete store implementations could diverge. Shared conformance tests run
  against simulated, PostgreSQL, and Turso stores.

### DST Compliance

- Canonical request digests use deterministic length-framed encoding and
  ordered maps.
- Journal, creation, and action idempotency identities derive deterministically
  from the durable reservation; no wall clock or ambient randomness is used.
- Recovery is an explicit bounded state machine with injected-failure and
  restart schedules in the simulated store.
- Service code uses the actor scheduler and existing scoped persistence traits;
  it introduces no filesystem, environment, network, thread, or wall-clock
  access in simulation-visible crates.

## Non-Goals

- Adding scope, tenant, principal, bundle, or pin selection to
  `DataOperationV1`.
- Copying a scoped pin onto a tenant-global actor.
- Making entity creation and action dispatch a cross-store transaction.
- Proving that no entity exists anywhere in the scope.
- Bootstrapping multiple entities in one operation.
- Replacing ordinary scoped typed module data after the first entity exists.

## Alternatives Considered

1. **Add scope selection to `DataOperationV1`** — rejected because it makes
   routing authority guest-controlled and weakens ADR-0191.
2. **Copy the active pin onto the tenant-global actor** — rejected because the
   caller's actor identity does not become scoped by invoking deployment.
3. **Use only the actor cache for idempotency** — rejected because eviction and
   restart lose the exact original receipt.
4. **Wrap creation and action in a cross-store transaction** — rejected because
   existing journals and dispatch have distinct commit points; simulated
   atomicity would be unsafe and misleading.
5. **Require an empty scope** — rejected because a collection-wide absence
   check races with concurrent creation and is unnecessary with dedicated
   authorization plus single-entity absence checks.
6. **Resolve retries through the current active pointer** — rejected because a
   later activation must not reinterpret a previously reserved operation.

## Rollback Policy

Disable installation and Cedar authorization of
`SchemaBootstrapDispatch`, then stop accepting new reservations. Retain existing
operation records and immutable bundle data so completed receipts remain
replayable and incomplete operations can be inspected or safely recovered.
Existing scoped actors and typed module data continue unchanged. Re-enabling
requires the same exact grant and policy; no stored pin is rewritten.
