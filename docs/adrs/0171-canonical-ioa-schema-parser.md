# ADR-0171: Canonical schema-backed IOA parsing

- Status: Accepted
- Date: 2026-08-30
- Deciders: Temper core maintainers
- Related:
  - Issue #85: Canonical IOA round-trip loses typed action parameters
  - ADR-0040: Composite actions
  - ADR-0041: IOA field invariants
  - ADR-0046: Unified action triggers
  - ADR-0049: State-entry timeouts
  - ADR-0181: Verified bounded collection workflows
  - `crates/temper-spec/src/automaton/types.rs`
  - `crates/temper-spec/src/automaton/parser.rs`

## Context

IOA source currently takes two incompatible parsing paths. A hand-written line parser
constructs the core automaton while a collection of Serde helpers reparses isolated
sections for triggers, composite metadata, action guards and effects, field invariants,
timeouts, collection workflows, keys, vectors, admission control, and webhooks. The
line parser ignores unknown lines and fields and drops unnamed state, action,
invariant, liveness, and integration blocks. The webhook helper additionally converts
any parse error into an empty list. Later verification cannot tell whether a
declaration was absent or discarded.

This split also makes canonical serialization unsafe. A typed action parameter written
inline serializes as `[[action.params]]`; the hand parser does not recognize that
nested table, so its `name` field overwrites the parent action name. A valid typed
failure route then fails bundle closure validation because its callback appears to be
missing.

The complete IOA data model already implements Serde deserialization. Maintaining a
second grammar in line-oriented code therefore adds loss without supplying a separate
capability.

## Decision

### Parse the complete document exactly once

`temper-spec` will deserialize the entire source directly into the canonical
`Automaton` schema with `toml::from_str`. Section isolation and parse-again extractors
are removed. TOML syntax and duplicate-key failures retain the source context reported
by the TOML deserializer.

**Why this approach**: one schema and one parse result make declaration consumption
structural. A supported declaration is represented in the AST once; malformed source
cannot be converted into an apparently valid partial automaton.

### Reject unknown schema content at its owning boundary

Closed IOA records and tagged variants use Serde's unknown-field rejection. This
includes automaton metadata, states, actions, safety and liveness declarations,
webhooks, trigger metadata, timeouts, collection workflows, keys, vectors, admission
control, guards, and effects. Required fields remain required, so truncated or unnamed
declarations fail during deserialization instead of disappearing.

The legacy `[[integration]]` record remains the deliberate extension point: keys not
owned by its fixed fields populate its existing string configuration map. Nested maps
such as trigger headers and parameters likewise retain arbitrary user-defined keys.

### Preserve supported source forms through schema deserialization

String-form guards and effects are part of the accepted IOA language and remain
supported by field deserializers on `Action`. Structured arrays continue through the
typed `Guard` and `Effect` schemas. Existing effect aliases, string booleans, composite
gate shapes, and collection copy-field forms remain accepted where the current parser
accepts them, but malformed values return an error rather than defaulting or being
omitted.

Typed `ActionParam` declarations retain their `name`, `type`, and optional
`entity_type` whether authored inline or emitted as nested canonical tables.

## Rollout Plan

1. Replace the parser and add regression coverage for typed parameters, unknown
   fields and tables, incomplete blocks, malformed webhooks, duplicate names, the
   valid repository corpus, and canonical serialize/parse round trips.
2. Run the complete workspace verification before deployment. Specs rejected by the
   stricter boundary must be corrected at their source; there is no permissive mode.
3. Update the blocked ARC consumer to the merged kernel commit and prove its typed
   module closure, immutable installation, failure callback, and deployed telemetry.

## Readiness Gates

- Every repository IOA fixture parses through the canonical public parser.
- Malformed or unsupported source fails before validation or runtime table generation.
- The public parser retains all declarations represented by the canonical schema.
- Canonical bundle recompilation preserves typed failure callback resolution.
- The downstream ARC application installs and exercises its typed failure route.

## Consequences

### Positive

- Verification can no longer pass a partial AST created by silent parser omission.
- Canonical TOML round trips preserve typed action parameters and action identity.
- New schema fields need one Serde model change rather than synchronized parser and
  extractor changes.
- Parallel parser and section-isolation code is deleted.

### Negative

- Previously ignored typos become deployment-blocking parse errors.
- Closed records require an explicit schema change before accepting new fields.

### Risks

- A historically accepted source form could be missed during migration. Repository-wide
  corpus coverage and explicit compatibility deserializers mitigate this without
  restoring permissive parsing.
- Trigger expansion mutates the parsed automaton. Idempotence tests prevent canonical
  reparsing from duplicating synthesized integrations or effects.

### DST Compliance

- Parsing is pure and deterministic. This change is confined to `temper-spec` and does
  not add clocks, randomness, concurrency, or simulation-visible I/O.

## Non-Goals

- Changing IOA runtime semantics or verification rules.
- Defining action-parameter nullability or requiredness; issue #80 owns that contract.
- Removing supported string-form guards or effects.
- Restricting intentional integration, header, parameter, or configuration maps.

## Alternatives Considered

1. **Recognize only `[[action.params]]` in the line parser** — rejected because every
   schema addition would still require synchronized grammars and another canonical
   round-trip bug could silently change behavior.
2. **Pre-scan for known text patterns before the current parser** — rejected because it
   cannot prove structural consumption and creates a third grammar.
3. **Keep per-section Serde extraction but propagate all errors** — rejected because
   unrelated declarations can still be omitted and cross-section duplicate or
   ownership errors remain invisible.

## Rollback Policy

Reverting restores permissive partial parsing and is therefore not a safe operational
fallback. If a valid supported source form is missed, extend its typed deserializer and
add a corpus regression while retaining the single strict document parse.
