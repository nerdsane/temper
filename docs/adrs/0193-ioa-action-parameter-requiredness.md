# ADR-0193: IOA Action-Parameter Requiredness

- Status: Proposed
- Date: 2026-08-30
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-spec` (IOA and CSDL action contracts)
  - `crates/temper-jit` (compiled transition metadata)
  - `crates/temper-server` (OData and actor dispatch)
  - module SDK generation and compatibility manifests

## Context

IOA action parameters currently lack an explicit absence contract. Callers can omit values or send JSON `null`, while guards and effects may silently substitute defaults or skip work. CSDL already has a nullability model, but its default is different from the desired IOA authoring contract: omitted CSDL `Nullable` means nullable, whereas callable IOA parameters should be required unless the spec explicitly opts into absence.

The ambiguity allows invalid input to reach guard evaluation, effect staging, event append, triggers, or lifecycle mutation. It also prevents generated SDKs and upgrade compatibility checks from describing the real action ABI.

## Decision

### IOA Owns Callable Action Intent

Plain string parameters and typed parameters without a `nullable` member are required. Typed IOA parameters may declare absence explicitly:

```toml
params = [
  "required_name",
  { name = "required_count", type = "Edm.Int64" },
  { name = "optional_note", type = "Edm.String", nullable = true },
]
```

Canonical IOA preserves explicit nullability metadata. Older transition-table fixtures that do not contain nullability deserialize as required. Temper does not reinterpret arbitrary CSDL: omitted CSDL `Nullable` continues to mean nullable according to OData semantics.

For every callable IOA action, bundle verification requires the matching bound CSDL action to have a non-nullable binding parameter of the correct entity type and the same normalized non-binding parameter names and nullability. Alias collisions are verification errors. Unrelated CSDL actions remain permitted. IOA parameter types remain descriptive metadata in this decision; CSDL stays authoritative for OData wire-type validation.

Stable verification detail codes are `csdl_action_parameter_requiredness_mismatch`, `csdl_action_binding_nullable`, and `nullable_action_parameter_consumed`.

**Why this approach**: IOA describes behavior and therefore supplies the intentional callable contract, while CSDL remains standards-compatible and exposes that contract to OData clients.

### Absence Must Have Defined Semantics

A nullable action parameter may be unused or passed through to a module input, where absence is serialized deterministically as JSON `null`. Verification rejects nullable parameters consumed by guards, state-mutating effects, spawn identity, required trigger mappings, or template substitutions. Default values and optional-consuming effects require a future explicit IOA construct.

**Why this approach**: allowing absence only at boundaries with defined representation prevents hidden fallbacks from changing behavior.

### Validate Before Semantic Work

One schema-driven validator serves OData bound actions and typed module-data actions. It uses the invocation's existing schema pin, excludes the binding parameter from the JSON body, and preserves each adapter's authorization order. Adapter validation occurs before actor dispatch.

Compiled transition metadata carries parameter type and nullability. The entity actor independently validates required inputs before reference checks, guards, effect staging, event append, triggers, or lifecycle changes, covering internal and optimized dispatch paths. Silent required-input fallbacks are removed.

HTTP failures use status 400 with `MissingActionParameter` for absent or null required values and `ActionParameterTypeMismatch` for other type failures. Module data uses `SchemaMismatch` with the same detail codes.

**Why this approach**: boundary validation gives precise client errors; actor validation makes the invariant true for every dispatch path.

### SDK and Upgrade Compatibility

Generated Rust SDKs map required parameters to `T` and explicitly nullable parameters to `Option<T>`. `None` crosses the existing data ABI as JSON `null`; the wire format does not change.

The existing structured action manifests remain the compatibility authority. Nullable-to-required is a breaking narrowing and rejects reuse with a diagnostic naming the fully qualified entity, action, parameter, and old/new nullability. Required-to-nullable is compatible for an existing required-value client only when the existing artifact-bound compatibility proof validates. This change does not introduce a parallel schema-diff mechanism or alter the module-data ABI.

## Rollout Plan

1. Land parser, canonical model, verifier, JIT metadata, runtime validation, generated SDK mapping, structured-manifest compatibility classification, and regression coverage in one Temper change.
2. Regenerate the existing `arc-agi-temper` draft PR #37, remove obsolete `Some(...)` wrappers for required inputs, rebuild WASM artifacts, and exercise HTTP, typed-client, persistence, restart, and upgrade paths.
3. Deploy downstream and verify rejected and accepted calls plus their Datadog traces before declaring the rollout complete.

## Readiness Gates

- All parser, bundle, semantic-lint, SDK, compatibility, adapter, actor, and end-to-end tests pass.
- Required-value rejection proves no state, event, trigger, effect, or lifecycle mutation.
- Repository formatting, clippy, dependency, determinism, pre-push, DST-review, and code-review gates pass.
- Downstream PR #37 is regenerated, deployed, and live-verified.

## Consequences

### Positive

- Action input contracts become explicit and consistent across specs, CSDL, generated clients, modules, and actors.
- Invalid calls fail before observable semantic work.
- SDK types and upgrade checks reflect real absence semantics.

### Negative

- Existing specs whose CSDL relied on implicit nullability must declare and regenerate the intended contract.
- Nullable values cannot yet participate in guards or mutations, even when an application could invent a local convention.

### Risks

- Generated or hand-authored bundles may expose latent IOA/CSDL mismatches. Deterministic diagnostics and downstream regeneration make the migration actionable.
- A missed dispatch path could bypass adapter validation. Actor-local validation is the defense in depth.

### DST Compliance

- Parameter metadata uses deterministic ordered collections and stable diagnostic sorting.
- Validation reads only pinned schema and message data; it does not use wall-clock time, randomness, threads, filesystem, network, or environment state.
- Rejection occurs before staging, so failed input cannot perturb simulated state or event order.

## Non-Goals

- Default values for omitted parameters.
- Optional parameter-consuming guards or effects.
- Changing CSDL's default `Nullable` semantics.
- Changing the module data ABI.
- Requiring exact alignment for CSDL actions that are not callable IOA actions.

## Alternatives Considered

1. **Make IOA parameters nullable by default** — rejected because it preserves ambiguous behavior and weak generated SDK types.
2. **Infer requiredness from parameter use** — rejected because contract meaning would change when implementation details change.
3. **Validate only in HTTP and module adapters** — rejected because triggers and internal dispatch can bypass them.
4. **Accept nullable values in effects with implicit defaults** — rejected because different consumers would acquire inconsistent absence semantics.

## Rollback Policy

Before downstream deployment, revert the Temper change and regenerate affected SDKs from the previous contract. After deployment, first roll back downstream artifacts and specs to the last compatible bundle, then revert the platform change. Persisted entity and event data are unchanged because this decision changes input validation and metadata, not storage or the wire ABI.
