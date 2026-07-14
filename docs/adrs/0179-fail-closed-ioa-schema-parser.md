# ADR-0179: Fail-closed, schema-backed IOA parsing

- Status: Accepted
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0040: Composite actions
  - ADR-0041: IOA field invariants
  - ADR-0046: Unified action triggers
  - ADR-0049: State-entry timeouts
  - `crates/temper-spec/src/automaton/types.rs`
  - `crates/temper-spec/src/automaton/parser.rs`
  - Linear ARN-214

## Context

IOA sources were parsed on two divergent paths. A hand-written line scanner
built the core automaton while separate Serde extractors re-read isolated
sections for triggers, composite metadata, field invariants, timeouts, keys,
vectors, admission, and webhooks. The line scanner ignored unknown lines and
tables and dropped unnamed state, action, invariant, liveness, and integration
blocks. Webhook extraction turned any parse failure into an empty list.

Later verification cannot tell “not declared” from “parser discarded it.” A
typo in a safety table or truncated assertion can therefore ship as a
seemingly valid partial automaton. The full `Automaton` data model already
implements Serde; a second grammar adds loss without capability.

## Decision

### Single full-document deserialize into the canonical schema

`temper-spec` deserializes the entire source once with `toml::from_str` into
`Automaton`. Parallel section extractors and the hand-rolled line grammar are
removed. TOML syntax and duplicate-key failures keep the spans reported by the
deserializer. String-form guards and effects stay as field-local embedded
languages; their compatibility decoders never rescan unrelated sections.

**Why**: one schema and one AST make declaration consumption structural.
Malformed source cannot become an apparently complete automaton.

### Unknown content is an error at the owning record

Closed records and tagged variants use Serde `deny_unknown_fields` (metadata,
states, actions, invariants, liveness, webhooks, triggers, timeouts, keys,
vectors, admission, guards, effects, field invariants). Required fields stay
required so truncated or unnamed blocks fail at deserialize time.

The legacy `[[integration]]` record remains the intentional extension surface:
keys outside its fixed fields populate its string config map. Nested maps such
as trigger headers and parameters keep arbitrary user keys.

### Supported legacy forms stay field-local

String guards/effects, effect type aliases, and string booleans remain accepted
through compatibility deserializers. Malformed values error instead of
defaulting or vanishing. Tooling (`verify_specs --syntax-only`) uses the same
canonical path.

## Rollout Plan

1. Add fail-closed regression tests (unknown tables/fields, incomplete blocks,
   malformed webhooks, duplicate keys, corpus round-trip).
2. Switch the public parser to schema-backed deserialize + uniqueness
   validation; correct in-tree specs that relied on silent drops.
3. No permissive mode. Specs that fail the stricter boundary must be fixed.

## Readiness Gates

- Repository IOA fixtures parse through the public parser and round-trip.
- Malformed or unsupported source fails before validation/table generation.
- Declarations represented by the schema are retained in the public AST.

## Consequences

### Positive

- Verification cannot pass a partial AST created by silent omission.
- New schema fields need one Serde model change, not a parallel grammar.
- Parallel extractor code is deleted.

### Negative

- Historical typos become hard parse errors.
- Closed records require an explicit schema change before new fields land.

### Risks

- A historically accepted form could be missed. Corpus coverage and explicit
  compatibility deserializers mitigate without restoring permissive parsing.

### DST Compliance

- Parsing is pure and deterministic. Confined to `temper-spec`; no clocks,
  randomness, concurrency, or simulation-visible I/O.

## Non-Goals

- Changing IOA runtime semantics or verification rules.
- Removing supported string-form guards/effects.
- Restricting intentional integration/header/parameter/config maps.

## Alternatives Considered

1. **More checks on the line parser** — still requires dual grammars and
   isolated re-parses for every schema addition.
2. **Pre-scan known text patterns** — cannot prove structural consumption;
   invents a third grammar.
3. **Keep per-section Serde extraction but surface errors** — unrelated
   declarations can still vanish; cross-section ownership stays invisible.

## Rollback Policy

Reverting restores silent partial parsing and is not a safe operational
fallback. If a valid supported form is missed, extend its typed deserializer
and add a corpus regression while keeping a single strict document parse.
