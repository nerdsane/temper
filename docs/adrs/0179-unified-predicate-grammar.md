# ADR-0179: One predicate grammar for every spec condition

- Status: Accepted
- Date: 2026-09-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0041: IOA field invariants (`[[field_invariant]]`)
  - ADR-0046: Unified action triggers (trigger guards)
  - ADR-0149: Free-boolean cross-entity guards in the verifier
  - ADR-0151: Guard identity in errors
  - ADR-0174: Strict action contracts (`[[action.constraints]]`, unchanged here)
  - Issue #500 (audit, proposal, plan), #502 (enforcement gaps)
  - `crates/temper-spec/src/predicate/`

## Context

A spec had four condition languages, plus two more around them:

1. **Action guards:** string clauses (`"is_true x"`, `"min x 3"`) or `{ type = ... }` tables, AND-only.
2. **Invariant asserts:** `&&`/`||`/`!`, `never()`, `ordering()`, gated by a separate `when`.
3. **Trigger guards:** a different vocabulary (`field`, `bool_true`, `cross_entity_state_in`) with `all_of`/`any_of`/`not` tables.
4. **Field invariants:** a fourth table grammar (`equals`, `absent`, `empty`, `any_of`) split into `when`/`require`.
5. `reactions.toml` guards: a copy of the trigger-guard tables.
6. `[[cross_invariant]]`: `related(Type, field).X in [...]`, in separate files.

Each consumer re-implemented its own subset: the JIT and the verifier could only represent an AND of atoms, the DST simulator only checked counter invariants on a counter named `items`, and about 23 invariants in repo specs were silently marked unverifiable because the assert parser did not understand them.

## Decision

### Sub-Decision 1: one expression string per condition slot

Action `guard`, trigger `guard`, reaction `guard`, `[[invariant]] assert` and `[[field_invariant]] assert` each hold one string in one grammar: `!`, `&&`, `||`, `=>`, comparisons, `in`/`not in`, `len()`, `empty()`, `null`, single-quoted strings and `Type[ref].status`. `when`/`require` are gone; implication replaces them. The reference is `docs/predicates.md`.

### Sub-Decision 2: one tree, one three-valued evaluator

`temper_spec::predicate` owns the parser, the canonical printer, name/type checking and the evaluator. Every consumer (JIT, verifier backends, server trigger and field-invariant paths, DST simulator) evaluates the same `Expr` through an environment trait. The evaluator uses Kleene logic: runtime environments know every value; the verifier reads values it does not model as unknown, so "may be enabled" is `!= false` and "is enabled" is `== true` under any combination of operators. This generalises ADR-0149's free booleans, which were only sound under AND.

### Sub-Decision 3: related entities read as statuses, missing as null

`Type[ref].status` reads the status of every related entity (`ref` holds one id or a list). An unset reference or a missing entity reads as `null`; a comparison must hold for every related entity. This reproduces the old `cross_entity_state` flags exactly (`required` becomes `!= null`, an optional allowlist becomes `empty(ref) || ...`, a denylist becomes `not in`). When the server's lookup budget runs out, the reference is marked unresolved and no guard reading it can pass.

### Sub-Decision 4: terminal states and history

`no_further_transitions` becomes `[automaton] terminal = [...]`. History predicates (`ordering()`) are dropped: the only use in the repo checked nothing.

### Sub-Decision 5: invariants must be provable

An `[[invariant]]` that reads something the cascade cannot model (a string or number variable, a field, a related entity) fails to load instead of being skipped with a warning. Such rules belong in `[[field_invariant]]`, which is enforced on writes. The twelve string invariants in repo specs moved there.

### Out of scope

`[[action.constraints]]` (ADR-0174) keep their own shape: they check request parameters before guards, are not modeled, and redact values in errors. `[[cross_invariant]]` keeps its grammar for now. Field-invariant enforcement gaps are tracked in #502.

## Rollout

- Specs in the old syntax fail to load with a hint; `temper migrate-predicates <files>` rewrites them in place (comments kept). All repo specs are converted.
- Stored tenant specs (`specs.ioa_source`) must be converted before a server running this version starts; TemperPaw converts its specs when it bumps its pinned `temper`.
