# ADR-0156: Immutable typed cross-entity reference contracts

- Status: Accepted
- Date: 2026-07-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0015: Agent OS cross-entity primitives.
  - ADR-0149: Free-boolean cross-entity guards in the L1 model check.
  - ADR-0150: Always-on composite cross-entity verification.
  - ADR-0153: Declared composite-key index and canonical key hashing.
  - `crates/temper-spec/src/automaton/` (IOA data model and validation).
  - `crates/temper-jit/src/table/` (runtime transition metadata).
  - `crates/temper-verify/src/composite/` (joint-state verification).
  - `crates/temper-server/src/entity_actor/` (pre-commit transition path).

## Context

Temper can express a cross-entity state guard, resolve a target entity for a
trigger, and validate CSDL navigation targets on OData writes. Those facilities
do not form a reference contract:

- IOA state variables and action parameters carry primitive string types, so a
  `workspace_id` can be accidentally populated with a `File` ID without a spec
  or runtime error.
- Action parameters are projected into entity fields after guard evaluation.
  Guards cannot compare an incoming parameter with an already-stored relation.
- Nothing generic prevents an action, patch, trigger, or optimized write path
  from replacing a relationship after creation.
- Declared keys provide a canonical, type-tagged hash for indexing, but cannot
  declare that the hash is also the entity's identity or validate that identity
  before a transition.
- CSDL relation checks run in the OData layer. Internal dispatch, direct actor
  actions, triggers, spawns, and composite sub-writes can bypass them.

The current code contains an entity-specific `Ref.Update` precondition in
`temper-server`. It proves that compare-before-transition is useful, but it is
not a platform primitive and cannot be verified from a generated spec.

ADR-0149 and ADR-0150 make cross-entity behavior visible to the verifier.
They do not establish which entity an ID is allowed to reference, whether that
reference may change, or whether an incoming action addresses the same related
entity as the current state. This ADR adds those missing contracts.

## Decision

### Sub-Decision 1: IOA owns scalar typed references

An IOA state variable may declare a scalar entity reference:

```toml
[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""
```

`entity_type` is required when `type = "ref"` and forbidden for other state
variable types. The target type must exist in the verified bundle. Reference
values are non-empty string entity IDs; the empty initial value means unset.
Collections of references are out of scope.

A typed reference is immutable by definition:

1. An unset reference may be assigned one non-empty value.
2. Repeating the same value is an idempotent write and is accepted.
3. Replacing the value, clearing it, or assigning a value of another reference
   type is rejected before state mutation or event append.

The IOA declaration is authoritative. CSDL cross-validation matches the IOA
reference name to the **dependent structural property** named by
`ReferentialConstraint.Property`, not to the navigation-property name. The
navigation's principal target type must equal `entity_type`, and
`ReferentialConstraint.ReferencedProperty` must name a key property on that
target. A contradiction fails bundle verification. CSDL navigation metadata is
not required merely to use a typed reference.

**Why this approach**: relation identity affects legal transitions and therefore
belongs in the behavioral spec. CSDL remains the derived OData projection rather
than a second source of behavioral truth.

### Sub-Decision 2: action parameters may be typed references

Typed action parameters use the same reference vocabulary:

```toml
params = [
  { name = "workspace_id", type = "ref", entity_type = "Workspace" }
]
```

The parser rejects missing target types, unknown target entities, and a parameter
whose name collides with a state reference of a different target type. Runtime
input must be a non-empty string and the referenced target must exist in the
current tenant.

Plain string parameters and existing state variables are unchanged. A string is
never implicitly promoted to a typed reference.

### Sub-Decision 3: `reference_equals` compares input with stored identity

Actions may require an incoming reference parameter to match a stored reference:

```toml
guard = [
  { type = "reference_equals", reference = "workspace_id", param = "workspace_id" },
]
```

The declaration is valid only when both operands are typed references to the
same entity type. At runtime the guard passes only when both values are present,
non-empty, and equal. An unset stored reference, missing parameter, wrong type,
or unequal ID fails the guard with a structured error naming both operands and
their declared entity type.

This is deliberately narrower than a general expression language. It covers
the identity-confusion class without introducing arbitrary I/O or related-field
reads into the pure transition evaluator.

**Why this approach**: comparison stays deterministic and actor-local. Target
existence is pre-resolved at the dispatch boundary; equality itself needs no I/O.

### Sub-Decision 4: one declared key may define deterministic entity identity

One declared key per entity may opt into the entity-ID contract:

```toml
[[key]]
name = "workspace_document"
properties = ["workspace_id", "document_id"]
entity_id = true
```

The key's properties must all be declared immutable typed references. Empty,
mutable, collection, counter, boolean, or undeclared properties are rejected by
the verification cascade.

The deterministic ID is the lowercase SHA-256 string returned by ADR-0153's
existing `canonical_key_hash(key_name, properties, fields)`. Temper does not add
a second hash format. A shared pure `derive_or_validate_entity_id` helper runs
at two boundaries:

- **before routing a create**, after its complete input field set is normalized
  but before choosing an actor key or persistence ID; it derives a missing ID or
  rejects a supplied mismatch;
- **inside the actor before commit**, over the fully materialized prospective
  state; it revalidates the already-routed ID and rejects any drift.

Existing-entity actions already have a routing ID and use only the actor-side
validation. Spawn, create/create-if-missing trigger, composite, data-only, and
native create paths must invoke the pre-routing helper before they construct a
target actor or journal identity. Additionally:

- every later write recomputes the hash over its fully materialized prospective
  state and rejects a mismatch before the transition;
- key fields remain immutable, so a successfully-created entity cannot drift to
  a different deterministic identity.

Entity IDs remain scoped by tenant and entity type, as they are today. The hash
does not add tenant or entity type bytes because those are already part of the
storage and actor identity, and changing the canonical form would break
ADR-0153 key-index parity.

### Sub-Decision 5: one pre-commit contract covers every write origin

The kernel compiles reference declarations, parameter types, equality guards,
and the identity key into `TransitionTable` metadata. An actor action uses a
two-pass evaluation inside its serialized turn:

1. Evaluate from-state and guards against current fields, normalized incoming
   parameters, and target-existence evidence resolved before the actor ask.
2. Apply every deterministic state/effect change to a staged clone, including
   parameter projection and other effect writes. Validate immutability, typed
   targets, and deterministic identity against this **fully materialized
   prospective state**. Only then replace live state and construct the event.

Target-existence evidence is strict: the target must already exist durably, or
its creation must be a sub-write in the **same atomic composite transaction** as
the source update. The composite preflight validates the new target's type and
derived ID, and the store commits both journal writes or neither. A planned
post-commit spawn, create trigger, callback, or integration is not existence
evidence because that effect may fail after the source event commits.

Consequently, legacy asynchronous `spawn store_id_in` may continue writing an
ordinary string field, but bundle verification rejects it when `store_id_in`
names a typed-reference or deterministic-key field. Apps that need an immutable
reference to a newly created child must use an atomic composite action containing
the child create and source reference assignment. Any other effect whose field
outcome cannot be materialized and committed atomically is likewise forbidden
from writing those fields.

Both passes finish before journal append or post-transition dispatch. A rejection
therefore produces no durable event, projection update, trigger, integration,
schedule, or spawn.

All state-changing entry points must use the same contract:

- OData POST, PATCH, and PUT;
- bound actions and direct entity actions;
- `EntityMsg::UpdateFields`;
- entity triggers and create/create-if-missing resolvers;
- spawn effects and composite sub-writes;
- data-only and native optimized create paths.

An optimized path may remain optimized only if it invokes the same validator.
Otherwise a contracted entity type is routed through the canonical actor path.
Delete validates incoming relation policy but does not reassign a reference.

Target existence is resolved outside the actor because actors do not perform
registry or store I/O during pure transition evaluation. Evidence is carried in
deterministic `BTreeMap`s, following the existing cross-entity-guard pattern. The
actor repeats every actor-local check immediately before commit, closing races
between concurrent first assignments.

The entity-specific `Ref.Update` contract is **not** migrated by this ADR.
`TargetCommitSha` is a mutable Git-ref compare-and-swap value, not immutable
ownership identity; declaring it as a typed reference would reject every valid
update. That Genesis-oriented hard-code is an existing kernel boundary violation
and must be tracked for separate extraction or a generic mutable-CAS primitive.
This ADR neither extends nor silently relocates it.

### Sub-Decision 6: structured failures are stable public behavior

Spec errors fail parsing, linting, or bundle verification before deployment.
Runtime failures use a typed `ReferenceContractViolation` with stable categories:

- `InvalidReferenceValue`;
- `ReferenceTargetMissing`;
- `ImmutableReferenceViolation`;
- `ReferenceEqualityViolation`;
- `DeterministicIdIncomplete`;
- `DeterministicIdMismatch`.

HTTP surfaces map semantic contract violations to `409 ConstraintViolation`.
Malformed parameter shapes remain `400 Bad Request`. Internal dispatch returns
the same category and detail in its failed response. Error detail names the
entity, operation/action, reference or key, expected target type/value, and
supplied value without exposing unrelated entity fields.

### Sub-Decision 7: verification uses a finite identity abstraction

The single-entity verifier does not enumerate arbitrary string IDs. For each
target entity type independently, it uses canonical symbolic equivalence
classes:

- every stored typed-reference slot is `Unset` or holds a symbol;
- for a transition with `R` stored slots, `P` typed parameters, and `E` atomic
  effect-created entities of that target type, symbols range over `0..R+P+E`;
- each parameter nondeterministically chooses `Unset`, any already-used symbol,
  or a new symbol, so parameters may equal any stored slot, equal each other, or
  be distinct from every existing value;
- each effect-created identity receives its declaration-ordered fresh symbol;
  `E` is statically bounded by the action's declared composite sub-writes, and
  asynchronous effects cannot introduce typed-reference symbols;
- after a step, symbols are canonicalized by first occurrence across
  declaration-ordered stored slots. States that differ only by symbol renaming
  collapse to the same state.

`R+P+E` symbols are sufficient: at most `R` distinct identities persist before
the step, at most `P` new identities arrive as input, and exactly the bounded
`E` identities may be created atomically by the action. Different target entity
types have disjoint symbol namespaces, so cross-type equality is impossible.
This defines initialization from an unset slot and comparisons where one
parameter participates in multiple equality guards.

Reachability explores every admissible equivalence pattern.
`reference_equals(reference = X, param = Y)` enables exactly the states where
both operands hold the same symbol. Accepted transitions may change an `Unset`
slot to a parameter/effect symbol, but never replace or clear a set symbol.

For `entity_id = true`, model state also carries
`IdBinding = Unbound | Bound(KeyTuple)`, where `KeyTuple` is the
declaration-ordered vector of canonical symbols for every deterministic-key
property:

- create may bind only after every key slot is set;
- a supplied or derived ID is abstracted as matching exactly that tuple;
- an accepted state with a complete key must have
  `IdBinding::Bound(current_key_tuple)`;
- staged validation rejects a different tuple, and reference immutability makes
  the bound tuple stable across later transitions.

The verifier proves tuple consistency, while runtime calls the real ADR-0153
hash. Hash collision resistance is the cryptographic assumption already accepted
by ADR-0153; the state checker does not model SHA-256 bits.

This extends ADR-0149's separation between "may fire" exploration and exact
local enablement: unknown environmental input broadens exploration, while safety
properties are checked on every resulting state. L0 encodes the same finite
cases symbolically; it does not turn equality into constant true or false.

The composite model carries the same per-target-type equivalence classes and
`IdBinding` tuples through reaction cascades. It remains a type/identity
abstraction, not an unbounded multi-instance database model.

### Sub-Decision 8: composite scope is the complete weak component

ADR-0150 intends one plan per weakly connected trigger-graph component. The
current implementation chooses the component's lexicographically smallest
entity as a seed, then builds the plan using only directed outgoing reachability.
For `Z -> A`, deterministic seed `A` produces scope `{A}` and silently omits
`Z` and its reaction.

Composite plan construction will accept the explicit weak-component member set
and include every internal edge, regardless of edge direction from the display
seed. The lexicographically smallest member remains the stable plan name only.
An isolated entity remains a singleton component.

**Why this is part of this ADR**: typed relations cannot be called verified if
the component planner can omit the entity that owns the relation.

### Sub-Decision 9: adding contracts to live types is activation-gated

Hot reload audits every existing entity of an affected type before activating a
new or changed reference/identity contract. The audit checks:

- stored reference shape and target existence;
- consistency of reconstructed historical values with set-once semantics;
- deterministic-ID equality for `entity_id = true`.

Iteration is deterministic and bounded. Any violation or exhausted audit budget
blocks activation with a report containing bounded entity IDs and violation
categories. Existing invalid data is not grandfathered and the old verified spec
remains active. Operators migrate the reported entities, then retry activation.

## Rollout Plan

1. **Phase 0 — ADR-only PR.** Accept this contract and mark the already-shipped
   ADR-0149/0150 decisions Accepted.
2. **Phase 1 — grammar and verification.** Add parser/types, bundle linting,
   finite L0/L1 abstractions, complete weak-component composition, and negative
   tests before runtime activation is possible.
3. **Phase 2 — canonical runtime enforcement.** Add transition metadata, target
   pre-resolution, actor-local pre-commit evaluation, structured errors, and
   keep the unrelated hard-coded `Ref.Update` contract unchanged and explicitly
   tracked as separate Genesis/CAS work.
4. **Phase 3 — write-path closure and activation audit.** Prove every write
   origin passes the contract, gate hot reload on deterministic existing-data
   audit, and exercise store/replay paths.
5. **Phase 4 — production proof.** Run the full workspace and DST suites, deploy,
   execute a live Parent/Child reference scenario, and verify accepted/rejected
   contract telemetry in Datadog.

## Readiness Gates

- Parser and bundle validation reject every malformed or cross-type declaration.
- No contracted entity can mutate a reference or commit a mismatched ID through
  any write origin.
- A reference to a newly-created target is committed only through an atomic
  composite transaction; asynchronous spawn failure cannot leave a dangling
  immutable reference.
- A rejected contract produces no event or post-transition effect.
- L0, L1, and composite verification agree on the finite identity abstraction.
- `Z -> A` weak-component coverage is regression-tested.
- Replay reproduces committed references and IDs without re-gating history.
- Hot reload refuses invalid existing data without replacing the active spec.
- Full workspace, DST, local live, deployed live, and Datadog checks pass.

## Consequences

### Positive

- Entity identity and relationship ownership become generated, verified
  contracts rather than naming conventions.
- Internal dispatch and optimized paths receive the same protection as HTTP.
- Apps can reject confused-deputy relation mismatches before any durable state
  change.
- Deterministic IDs reuse an existing canonical format and index declaration.
- Composite verification finally covers every member promised by ADR-0150.

### Negative

- Action evaluation gains parameter context and reference metadata.
- Contracted writes may require bounded target-existence reads before the actor
  ask.
- Activating a contract on a populated type requires an audit and possibly a
  migration.
- The verifier gains another finite abstraction and associated state-space cost.

### Risks

- A write-path bypass would make enforcement inconsistent. Mitigation: one
  validator, a fast-path eligibility check, and a matrix test over every origin.
- External target deletion may race target-existence pre-resolution. Existing
  relation delete policy remains responsible for cross-entity serialization;
  this ADR does not claim distributed transactions.
- Large activation audits may exhaust their budget. They fail closed and report
  incomplete rather than silently activating.
- Hash drift would corrupt identity. The implementation must call the ADR-0153
  function directly and add write/read/ID parity tests rather than duplicate it.

### DST Compliance

- Reference and parameter abstractions use `BTreeMap`/`BTreeSet` and stable
  declaration order.
- Hashing is the existing deterministic SHA-256 canonicalization.
- Actor-local validation uses no clock, random source, thread, filesystem,
  network, or environment variable.
- Target reads remain behind existing store/registry boundaries and simulation
  supplies deterministic evidence.
- Counterexamples and activation-audit reports sort entity IDs and categories.

## Non-Goals

- Collections of typed references.
- Arbitrary expressions or comparisons across fields on two loaded entities.
- Automatically converting legacy string fields into references.
- Cross-tenant references.
- A distributed transaction spanning reference source and target entities.
- Changing the ADR-0153 canonical hash format.
- Replacing or relocating the mutable Git `Ref.Update` compare-and-swap contract.

## Alternatives Considered

1. **CSDL navigation properties as the source of truth** — rejected because
   transition legality belongs in the behavioral spec and internal dispatch does
   not naturally flow through CSDL handlers.
2. **A separate relation specification file** — rejected because it creates a
   third model beside IOA and CSDL and weakens generated-spec ownership.
3. **General guard expression language** — rejected for this slice; it expands
   parsing, verification, and I/O semantics far beyond identity equality.
4. **UUIDv5 or string templates for deterministic IDs** — rejected because
   ADR-0153 already defines a canonical, type-tagged key hash.
5. **Require callers to always supply deterministic IDs** — rejected because
   derivation is unambiguous and centralized derivation prevents client drift.
6. **Grandfather existing invalid entities** — rejected because future writes
   would inherit unverified identity and make activation status misleading.
7. **Validate only at HTTP boundaries** — rejected because triggers, composite
   writes, and direct dispatch are first-class platform paths.

## Rollback Policy

Before a spec declares typed references or `entity_id = true`, the implementation
is additive. Removing those declarations returns the entity to legacy behavior
after a successful spec activation.

After entities are created with deterministic IDs, their IDs are durable public
identity and are not rewritten during rollback. Rolling back enforcement requires
removing the declarations and derived metadata, not re-keying journals. If the
feature itself must be removed, keep parsing the declarations as rejected
unsupported syntax so a previously-contracted spec cannot silently load without
its guarantees.
