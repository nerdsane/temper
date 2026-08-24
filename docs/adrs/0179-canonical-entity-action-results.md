# ADR-0179: Canonical Entity-Valued Action Results

- Status: Accepted
- Date: 2026-08-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0142: Dispatch Acknowledges After Projection
  - ADR-0154: OData Read-Surface Truthfulness
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - `crates/temper-codegen/src/module_sdk.rs`
  - `crates/temper-server/src/application_data/`
  - `crates/temper-wasm-sdk/src/data/`

## Context

Generated module clients currently map every non-EDM, non-enum action return
type to a transparent string ID newtype. For an action whose CSDL return type is
an entity, the governed application-data service instead returns the actor's
raw post-action `state.fields` object.

The action is already durably committed when the generated client attempts to
decode that object as a string. The resulting
`GeneratedResultTypeMismatch` discards the successful typed acknowledgement,
including its commit token, and can make a caller retry an action that already
ran. The raw object also is not guaranteed to match the canonical entity shape
used by keyed reads: it may omit or mis-case the ID, state, counters, or declared
CSDL fields.

Entity-valued results therefore need one schema-derived generated type and one
host-side canonical representation shared with authoritative keyed reads.

## Decision

### Generate Entity Return Types as Entity Structs

When a bound action's CSDL `ReturnType` resolves to its bound entity type, the
generated method returns `TypedAction<GeneratedEntity>`. EDM scalars, named
enums, nullable results, void results, and deliberately omitted results keep
their existing ABI representation and behavior.

Entity resolution uses the same fully qualified CSDL identity used to generate
entity clients. It does not infer entity-ness from spelling or fall back to an
ID newtype. A bound action returning a different entity type fails SDK
generation: the current action ABI carries the committed state of the bound
actor and cannot truthfully fabricate another entity's authoritative value.

**Why this approach**: the declared return type is the entity value, not a
reference. Generating the entity struct makes the Rust contract match the CSDL
contract and the host payload.

### Canonicalize Through the Authoritative Entity Projection

For an entity-valued action result, the application-data host returns the
canonical post-action entity object produced by the same schema-aware projection
as a sequence-aware keyed read. The object contains the entity ID, automaton
state, counters, and declared fields under exact CSDL property names. The
action response uses the committed actor state directly and must not wait for
an eventually consistent query projection.

Non-entity action results retain their current result semantics. The response
budget may still deliberately omit any committed result; omission remains
explicit through `result_omitted` while the commit token is preserved.

**Why this approach**: one projection prevents read/action drift and lets an
action acknowledgement be decoded immediately without a polling round trip.

### Treat the Commit Acknowledgement as Authoritative

The SDK records the action's commit token before typed result decoding, as it
does today at the raw envelope boundary. Generated entity-result tests must
prove that a successful action returns the typed acknowledgement and that the
same client automatically applies the observed sequence to a following keyed
read.

The implementation eliminates the known schema mismatch rather than adding a
raw-envelope fallback. A caller never needs to retry a committed action merely
to recover its post-action entity value.

**Why this approach**: retries are unsafe after an acknowledged state
transition. Preserving the normal typed success path gives callers both the
canonical value and the sequence needed for safe follow-up reads.

## Rollout Plan

1. Ship code generation, host canonicalization, and end-to-end regression
   coverage together in one change.
2. Validate the generated SDK, host/SDK parity, response compaction, artifact
   binding, full workspace, lint, and deterministic-simulation gates before
   merge.
3. Verify the deployed application-data path with an entity-valued action and
   inspect Datadog telemetry for one action dispatch and a sequence-aware keyed
   read.

## Readiness Gates

- An entity-valued generated action returns `TypedAction<Entity>`.
- Its canonical value includes ID, state, counters, exact CSDL casing, and all
  declared fields.
- The action commits once and the returned token drives a non-polling keyed
  read at or above the committed sequence.
- Scalar, enum, nullable, void, and omitted results do not regress.
- Generated source and artifact bindings remain deterministic.

## Consequences

### Positive

- Generated client types match entity-valued CSDL action contracts.
- Action acknowledgements and keyed reads expose one canonical entity shape.
- Callers retain commit tokens and avoid unsafe retries after successful writes.

### Negative

- Host action result construction becomes schema-aware.
- Entity return types must match the bound entity until the action ABI can
  carry a separately materialized authoritative entity result.

### Risks

- A second projection implementation could drift from keyed reads. The
  implementation mitigates this by extracting and reusing one canonical helper
  and by asserting byte-for-value parity in tests.
- Response compaction can omit large entity values. The existing explicit
  `result_omitted` contract and commit token remain the recovery mechanism.

### DST Compliance

- Canonicalization iterates schema metadata and counter state in deterministic
  order and introduces no clocks, randomness, ambient I/O, threads, or
  unordered collections.
- The action response is derived from the committed actor state at its exact
  durable sequence.
- No `// determinism-ok` suppression is expected.

## Non-Goals

- Changing public OData action response semantics.
- Adding raw-envelope escape hatches to generated clients.
- Changing scalar, enum, nullable, void, or response-compaction ABI shapes.
- Adding entity-specific state names or application logic to the kernel.
- Materializing a different entity instance as the result of a bound action.

## Alternatives Considered

1. **Decode entity results as ID newtypes** — Rejected because the CSDL declares
   a value and the host needs to return the committed canonical entity.
2. **Return only the commit token and force a keyed read** — Rejected because it
   changes declared action semantics and adds an unnecessary round trip.
3. **Expose a raw-envelope fallback after decode failure** — Rejected because it
   weakens generated guarantees and leaves committed-success handling ambiguous.
4. **Maintain a separate action-result serializer** — Rejected because it would
   inevitably drift from canonical keyed-read casing and field projection.

## Rollback Policy

Revert the generator and host changes together. Do not retain one side alone:
the generated result type and host payload are a single versioned contract.
