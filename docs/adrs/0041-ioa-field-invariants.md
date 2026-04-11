# ADR-0041: IOA field invariants + cross-invariant parent-field lookups

- Status: Accepted
- Date: 2026-04-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0001: Immediate rollout of cross-entity safety controls (introduces `[[cross_invariant]]` and the `related(...).status in [...]` grammar this ADR extends)
  - `docs/plans/ioa-field-invariants.md` — the implementation plan that materialised into this change
  - `crates/temper-spec/src/automaton/` — IOA spec data model and parsers
  - `crates/temper-spec/src/cross_invariant/` — cross-invariant spec data model and parser
  - `crates/temper-server/src/odata/` — OData write-path wiring

## Context

Temper's spec grammar cannot express **cross-field validation** — rules whose truth depends on the combination of two or more fields on the same entity, or on a scalar property of a related entity other than its status. The motivating use case came while planning the Crucible reference app, which needs to reject `POST /tdata/Environments` when `ConfigType == "Local"` and any of `NetworkingType`, `AllowMcpServers`, `AllowPackageManagers`, `AllowedHosts`, or `Packages` are set. No existing mechanism can state that rule:

- **IOA guards** (`crates/temper-spec/src/automaton/toml_parser/guards.rs`) operate on IOA state variables (`state_in`, `min_count`, `is_true`, `list_contains`, `cross_entity_state`). Guards do not read CSDL properties from `initial_fields`, and guards only fire on action transitions — OData `POST`/`PATCH` do not invoke an IOA action, so there is no transition point for a guard to attach to.
- **Cross-invariant assertions** (`crates/temper-spec/src/cross_invariant/parser.rs`) only parse `related(TargetEntity, sourceField).status in [...]`. The `.status` accessor is hard-coded and the only supported operator is `in`. No arbitrary scalar field lookup, no `not in`.
- **CSDL `BaseType`** is silently ignored (`crates/temper-spec/src/csdl/types.rs` has no `base_type` field), so type specialisation with different field sets per subtype is not an option.
- **Cedar** is not evaluated on OData entity-set `POST`.

Meanwhile `run_write_prechecks` (`crates/temper-server/src/odata/common.rs`) already runs a two-step pipeline over `fields: &serde_json::Value` — `pre_upsert_relation_checks` then `post_write_invariant_checks` — which is the natural wiring point for a new check step.

This ADR is **platform work only**. The Crucible reference app that consumes the new grammar lives on a separate branch and will be replanned once this change lands.

## Decision

Introduce a small, composable grammar for cross-field validation and extend the existing cross-invariant parser to read arbitrary scalar properties on related entities.

### Sub-Decision 1: `[[field_invariant]]` in IOA specs

A new top-level section in IOA TOML files, parsed into a `FieldInvariant` struct in `temper-spec`. Each invariant has a `name`, a `when` predicate, a `require` predicate, and an optional `message`. When the `when` predicate matches the entity's `initial_fields` snapshot, the `require` predicate must also match or the write is rejected with HTTP 409.

**Leaf predicates.** Each leaf inspects exactly one field. The grammar is deliberately minimal: two truly atomic leaves (`absent` and `equals`) plus one convenience leaf (`empty`) because collection-emptiness is common enough to warrant a shortcut.

| Predicate | Atomic? | Passes when |
|---|---|---|
| `{ field = X, absent = true }` | atomic | key `X` is missing from the payload, or its value is `null` |
| `{ field = X, equals = V }` | atomic | key `X` exists and equals `V` (bool, string, number) |
| `{ field = X, empty = true }` | convenience (equivalent to `any_of [absent, equals "", equals []]`) | key `X` is absent, `null`, `""`, or `[]` |

Derived forms — no dedicated leaves:

- **field is present** → `{ not = { field = X, absent = true } }`
- **field is not V** → `{ not = { field = X, equals = V } }`
- **field is absent or false** → `{ any_of = [ { field = X, absent = true }, { field = X, equals = false } ] }`

**Combinators.** Predicates compose via explicit logical operators:

| Combinator | Semantics |
|---|---|
| `{ any_of = [ p1, p2, ... ] }` | OR — passes if any child passes. Short-circuits on first pass. |
| `{ all_of = [ p1, p2, ... ] }` | AND — passes if every child passes. Short-circuits on first fail. |
| `{ not = p }` | NOT — passes if the child fails |

A bare predicate is its own base case; no wrapping required. Both `when` and `require` accept either an atomic predicate or a combinator — the two positions use the same grammar so tooling and evaluation are identical.

**Evaluation rules.**
- Evaluated on **create** (`POST /tdata/{EntitySet}`) and **update** (`PATCH /tdata/{EntitySet}('<id>')`) against the post-write `initial_fields` snapshot.
- Not evaluated on `DELETE` — the state machine's terminal-state invariants already govern delete semantics.
- Not evaluated on bound actions — action transitions are already guard-protected.
- Violation returns HTTP **409 ConstraintViolation** with `error.details.type == "field_invariant"`, `error.details.invariant == <name>`, and `error.message == <configured message>` (or a generic fallback if `message` is omitted).
- Respects the existing `state.cross_invariant_enforce` feature flag for consistency with other constraint checks.

**Example.**

```toml
[[field_invariant]]
name = "LocalNetworkingMustBeUnrestricted"
when = { field = "ConfigType", equals = "Local" }
require = { field = "NetworkingType", equals = "Unrestricted" }
message = "Local environments must use Unrestricted networking"

[[field_invariant]]
name = "LocalCannotAllowMcpServers"
when = { field = "ConfigType", equals = "Local" }
require = { any_of = [
  { field = "AllowMcpServers", absent = true },
  { field = "AllowMcpServers", equals = false },
]}
message = "Local environments cannot set allow_mcp_servers"
```

**Why this approach.** Composable atoms (`absent`, `equals`) plus explicit combinators (`any_of`, `all_of`, `not`) cover every future case with a small surface. New compound predicates such as `absent_or_false` accumulate without bound; combinators do not.

### Sub-Decision 2: Extend cross-invariant assertions to arbitrary scalar fields and `not in`

Cross-invariants live in a standalone `cross-invariants.toml` file. The existing assertion grammar only parses `related(TargetEntity, sourceField).status in ["A", "B"]` — the `.status` accessor is hard-coded and the only operator is `in`. Two extensions:

1. **Arbitrary scalar field after the `.`** — accept any identifier, not just `status`. The default stays `status` so existing specs continue to parse.
2. **`not in` operator** — accept `not in [...]` alongside `in [...]`.

Extended grammar:

```
related(TargetEntity, sourceField).<FieldName> in  [<literal>, ...]
related(TargetEntity, sourceField).<FieldName> not in [<literal>, ...]
```

Where `<FieldName>` is any valid identifier and `<literal>` is a double-quoted string. The parsed struct gains a `field_name: String` and an `operator: CrossInvariantOperator { In, NotIn }`, both with `status` / `In` defaults for backwards compatibility.

**Example (Crucible child → parent).**

```toml
[[invariant]]
name = "AllowedHostRequiresNonLocalParent"
kind = "hard"
on = "EnvironmentAllowedHost.*"
assert = 'related(Environment, EnvironmentId).ConfigType not in ["Local"]'
```

Reads as: on any action on `EnvironmentAllowedHost`, load the `Environment` whose id is in the child's `EnvironmentId` FK; reject unless the parent's `ConfigType` is anything *other than* `Local`.

**Why this approach.** Since OData deep insert is not supported (`crates/temper-server/src/odata/write.rs`), child entities are POSTed separately. Each child's own write path must look up its parent via a cross-invariant and reject if the parent is in a disallowed shape. Extending the existing grammar is a smaller change than inventing a second mechanism.

**Operator coverage.** Only `in` and `not in` are added in this PR. `==`/`!=` are redundant with single-element `in`/`not in`. `<`/`<=`/`>`/`>=` and `is null` are YAGNI — no currently-planned reference app needs them. `contains`/`starts_with`/regex belong to a future expression-language proposal.

### Sub-Decision 3: Server wiring

- A new check step `pre_upsert_field_invariant_checks` is inserted in `run_write_prechecks` between `pre_upsert_relation_checks` and `post_write_invariant_checks`. It iterates the per-tenant `field_invariants` list in declaration order, short-circuits on the first violation, and honours the `state.cross_invariant_enforce` feature flag.
- `ConstraintViolationType` gains a `FieldInvariant` variant that maps to the `"field_invariant"` string in the JSON error body.
- The existing cross-invariant evaluator is extended to read `<FieldName>` instead of hard-coded `status`, and to support `not in` via the new operator field. Backwards compatibility for callers using the old `related(...).status in [...]` form is total.
- `SpecRegistry` stores parsed `field_invariants` per tenant per entity type in a `BTreeMap` (deterministic iteration) and exposes `field_invariants_for(tenant, entity_type)`.

## Rollout Plan

1. **Phase 0 (this PR)** — Ship the spec extensions, server wiring, registry load path, and an integration test covering every leaf, combinator, and backwards-compatibility case. Additive, feature-flag-free. No existing specs use the new grammar, so there is no migration.
2. **Phase 1 (follow-up)** — Replan and ship the Crucible reference app on top of the new grammar. Crucible is the first consumer; it lives on a separate branch today.

## Consequences

### Positive
- Reference apps can declare cross-field validation rules in specs instead of writing handler code, preserving the spec-driven philosophy.
- The new grammar is small, composable, and covers every motivating case without accumulating compound predicates.
- Backwards compatibility for existing cross-invariants is total — no spec migration, no behaviour change for unchanged specs.

### Negative
- Spec grammar gains logical operators, which is a meaningful surface increase. The ADR documents a hard boundary: no arithmetic, no string functions, no multi-field antecedents. That boundary must hold or the grammar will drift into a general-purpose expression language.
- Constraint violations gain a new JSON error shape (`details.type == "field_invariant"`) that clients may need to handle.

### Risks
- A future contributor adds a fourth leaf (e.g. `greater_than`) without going through ADR review. Mitigation: the parser rejects unknown predicate keys with an explicit error, and every new leaf must update the ADR's leaf table.
- An agent-authored spec writes a trivially-unsatisfiable invariant (e.g. `when == require`). Mitigation: the lint pass rejects empty combinators; semantic unsatisfiability is deferred to the cascade.

### DST Compliance
This ADR touches simulation-visible crates (`temper-server/src/odata/constraints.rs`, `temper-server/src/odata/common.rs`, `temper-server/src/registry/`). Determinism is preserved by:

- Using `BTreeMap`/`Vec` for all new registry storage and iteration (no `HashMap`/`HashSet`).
- Confining evaluation to pure `serde_json::Value` inspection and iteration over a pre-built `Vec<FieldInvariant>` — no wall clock, no `std::fs`/`std::net`/`std::env`, no threading primitives, no `OsRng`, no `chrono::Utc::now`.
- Reusing the existing `Instant::now` duration measurement in `post_write_invariant_checks` only — the new code adds no new clock reads.

The 25-pattern determinism guard runs as part of the pre-commit gate.

## Non-Goals

- **General-purpose expression language.** No arithmetic, string functions, or multi-field antecedents beyond what `all_of` composes. These belong to a future ADR if a real spec needs them.
- **Comparison operators beyond `in`/`not in`** on the cross-invariant side. No `==`, `!=`, `<`, `<=`, `>`, `>=`, `is null`.
- **Moving cross-invariants into the IOA file.** They stay in `cross-invariants.toml` per existing convention.
- **Schema changes to `CrossInvariant`** beyond the parser-side `field_name`/`operator` additions. No new `message` field, no per-invariant `delete_policy`.
- **Deep insert support** in the OData write handler.
- **Post-`PATCH` cross-entity sweep** — `PATCH`ing a parent such that its existing children now violate a cross-invariant is a known limitation and is out of scope for this ADR.
- **Dropping the old `related(...).status in [...]` syntax.** Kept for backwards compatibility indefinitely.

## Alternatives Considered

1. **Extend guards to read `initial_fields` + route OData `POST`/`PATCH` through implicit `Create`/`Update` IOA actions.** Rejected: retroactively changes how every OData write works in every reference app; `PATCH` does not fit cleanly into the "guard protects a transition" model (it is a generic field update, not a state transition). Field invariants are a smaller, localised change that keeps the state machine and row-shape-validity concerns separate.
2. **CSDL `BaseType` specialisation** (e.g. separate `LocalEnvironment` / `CloudEnvironment` entity types). Rejected: the CSDL parser does not support `BaseType`, and splitting a single logical entity into subtypes breaks the single-endpoint API shape.
3. **Cedar at write time.** Rejected: Cedar is not evaluated on OData entity-set `POST`.
4. **Custom handler code per reference app.** Rejected: violates the spec-driven philosophy; reference apps should not hand-write HTTP handlers.
5. **Hard-coded compound predicates** such as `absent_or_false`, `absent_or_empty`, `present`, `not_equals`. Rejected: compound shapes accumulate forever. Composable atoms plus combinators cover every future case. The single exception — `empty` — is retained as a convenience leaf because collection-emptiness is common enough to warrant the shortcut, and is documented as non-atomic.
