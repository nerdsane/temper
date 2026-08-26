# ADR-0189: Typed WASM State Boundaries

- Status: Proposed
- Date: 2026-08-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - `crates/temper-wasm-sdk/src/context.rs`
  - `crates/temper-wasm-sdk/src/schema_deployment.rs`
  - `crates/temper-codegen/src/module_sdk/`

## Context

Temper exposes one entity through three representations. IOA state and WASM
guest models use `snake_case`; CSDL and OData use canonical schema names such
as `PascalCase`; and the runtime invocation snapshot separates ordinary
fields, counters, booleans, and lists. Application modules currently bridge
those representations by inspecting `serde_json::Value`, probing multiple
names, and accepting both nested and flat state shapes.

That tolerance hides contract drift. A module can continue working when its
IOA, CSDL, or host envelope no longer agrees with the representation it was
compiled against. It also repeats runtime-envelope policy in every module.
This is inappropriate for the v0.12 hard cut: invalid legacy shapes must fail
at the boundary instead of becoming permanent compatibility behavior.

ADR-0157 already establishes the other half of the boundary. Generated module
data clients expose idiomatic Rust field names while explicit Serde renames
bind them to exact CSDL property names. The SDK does not yet provide the same
typed entry point for the invoking member state or a migration's source state.

## Decision

### Decode runtime envelopes once in `temper-wasm-sdk`

The WASM SDK will expose a generic member-state decoder and a convenience
method on `Context`. The decoder accepts the exact runtime envelope containing
`fields`, `counters`, `booleans`, `lists`, and top-level lifecycle `status`.
It constructs one logical object and deserializes that object into the
guest-supplied Rust type.

Merge precedence is explicit and deterministic:

1. ordinary `fields` establish the base object;
2. `counters`, `booleans`, and `lists` overwrite their projected copies in
   that order; and
3. top-level `status` overwrites any projected lifecycle field.

The overwrite is not compatibility probing. The runtime deliberately projects
typed state into `fields` for query use while retaining authoritative typed
sections. The typed sections and top-level lifecycle status therefore own the
member-state value.

The decoder does not inspect alternate spellings, transform names, or accept a
flat object in place of the envelope. Guest structs use the exact snake_case
IOA names they consume. Missing required fields and type mismatches are normal
Serde failures surfaced as a typed boundary error.

### Decode migration source state without name adaptation

The schema-migration service will use the verified source CSDL contract to
canonicalize every stored property to its IOA snake_case name before producing
`canonical_state_json`. Runtime-owned identity and lifecycle projections become
`id` and `status`. Unknown properties, ambiguous contract mappings, and
conflicting duplicate projections are rejected. Migration input does not retain
PascalCase CSDL or runtime aliases.

`SchemaMigrationInputV1` will expose a generic source-state decoder for that
`canonical_state_json`. Migration code supplies a small snake_case Rust struct;
the SDK parses the canonical JSON directly into that type. It does not probe
PascalCase aliases or envelope shapes because migration source state is already
the canonical logical object.

### Keep CSDL/OData mapping explicit

Generated application-data models continue to use idiomatic snake_case Rust
members with explicit `#[serde(rename = "CanonicalCsdlName")]` attributes.
The entity decoder accepts exact CSDL names only. Regression tests will cover
that a snake_case wire property is rejected when the generated model requires
its PascalCase CSDL property.

The member-state and OData models remain separate Rust types even when they
describe the same entity. Sharing a permissive `Value` model across those
boundaries is prohibited.

### Keep generation contract-ready

The generic decoder is the stable kernel boundary used by hand-written small
state structs now and by generated IOA member-state structs later. This change
does not infer entity-specific fields in the runtime or hardcode application
state names. A later code-generation change may emit those structs from the
verified IOA+CSDL closure without changing the decoding contract.

## Rollout Plan

1. Add the SDK boundary types, member-state envelope decoder, and migration
   source-state decoder with hard-cut rejection tests.
2. Add generated-client regression coverage for exact CSDL property names.
3. Exercise the SDK through a real WASM invocation context and a pure schema
   migration locally before release.
4. Publish the kernel release and migrate application modules to the typed
   API, deleting their multi-name and multi-shape helpers.

## Readiness Gates

- Flat legacy member state is rejected.
- Snake_case IOA fields, counters, booleans, lists, and lifecycle status
  deserialize into one typed member-state model.
- Migration source state deserializes directly into a snake_case model.
- Generated OData entity decoding requires exact CSDL property names.
- `temper-wasm-sdk`, `temper-codegen`, `temper-wasm`, and workspace tests pass.
- A locally invoked WASM module and pure migration demonstrate the boundaries
  end to end.

## Consequences

### Positive

- Application code no longer owns runtime-envelope flattening.
- Representation drift fails at a single typed boundary with a useful error.
- IOA/WASM and CSDL/OData casing remain explicit instead of heuristic.
- Future generated member-state bindings can reuse the same SDK contract.

### Negative

- Modules that relied on flat member state or alternate spellings must migrate
  for v0.12.
- Member-state and OData DTOs intentionally duplicate some field declarations
  until IOA member-state generation is implemented.

### Risks

- Runtime `fields` can contain projected copies of typed state. An accidental
  precedence change could expose stale values. Fixed merge order and tests
  ensure the authoritative typed sections always win.
- Generic Serde errors can expose implementation-oriented paths. The SDK wraps
  them with boundary-specific context while retaining the useful field name.

### DST Compliance

The boundary lives in `temper-wasm-sdk` and performs no I/O, time access,
randomness, or concurrent work. Envelope merging follows a fixed section order.
No determinism suppression is required.

## Non-Goals

- Changing the public OData wire format.
- Preserving legacy flat or alternate-casing member-state inputs.
- Hardcoding application-specific state models in the kernel.
- Generating IOA member-state structs in this change.

## Alternatives Considered

1. **Keep application helpers** — Rejected because every module would retain
   different precedence, casing, and compatibility behavior.
2. **Normalize every key heuristically in the SDK** — Rejected because casing
   is contract metadata and normalization can hide collisions or schema drift.
3. **Use one DTO for member state and OData** — Rejected because the two wire
   contracts intentionally use different names and shapes.
4. **Flatten in the server invocation payload** — Rejected because it would
   erase the authoritative typed sections and make the host ABI less explicit.

## Rollback Policy

Before the v0.12 release, revert the SDK API and its callers together. After
release, do not restore alias probing; fix an invalid producer or issue a new
versioned boundary contract.
