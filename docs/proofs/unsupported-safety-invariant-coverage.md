# Safety Invariant Capability Coverage

This proof records the verification capability boundary introduced by ADR-0190.
Every `[[invariant]]` assertion is translated once into the shared
`InvariantKind` IR. An assertion is model-proved, explicitly attached to typed
runtime enforcement contract version 2, or becomes `Unverifiable`.
`Unverifiable` is a hard `TVE001` error and no cascade backend runs.

## Checked-in corpus replay

Replay date: 2026-07-14.

The automated corpus test recursively reads every `.ioa.toml` file under
`crates/`, `os-apps/`, and `reference-apps/`. For every declaration it verifies
one of two outcomes:

1. the assertion has a typed model-proved or runtime-enforced `InvariantKind`; or
2. the cascade fails before L0 with `TVE001`, the invariant name and assertion,
   and an exact half-open source span whose bytes equal the assertion text.

Command:

```console
cargo test -p temper-verify --test production_invariant_corpus -- --nocapture
```

Recorded result:

```text
production invariant corpus: 110 specs, 120 declarations,
120 supported, 0 rejected, 53 distinct forms
test result: ok. 1 passed; 0 failed
```

## Supported proof forms

| Form | Shared IR guarantee |
| --- | --- |
| Declared boolean / `!boolean` | Required truth value is checked by every state evaluator. |
| `is_true boolean` | Normalized to the same declared-boolean IR before verification. |
| `true` | Normalized to an empty conjunction, the typed identity for unconditional truth. |
| `string != ''` | Enforced atomically on tentative state before persistence and replay publication. |
| `counter_a OP counter_b` | Enforced atomically on tentative counters before persistence and replay publication. |
| `counter > N`, `>= N`, `< N`, `<= N`, `== N` | Literal comparison is represented directly and checked by SMT, Stateright, simulation, and property tests. |
| `no_further_transitions` | Trigger states have no enabled transition. |
| `never(State)` | The forbidden state is unreachable. |
| `&&`, `||`, parentheses | Typed child expressions are composed recursively; one unsupported child rejects the whole declaration. |

The current corpus exercises declared booleans, every listed literal counter
operator, `no_further_transitions`, and compound `&&`/`||` expressions.

## Rejected production forms

None. All 53 checked-in assertion forms are either model-proved or attached to
runtime enforcement contract version 2. Unknown syntax remains a `TVE001`
capability error before any backend, cache, or trust decision.

The parser also recognizes `ordering(Before, After)`, but the shared verifier
IR rejects it until event-history semantics are represented by every backend.

Rejection is independent of reachability, simulation seed count, property-test
case count, and fail-fast configuration. Direct SMT, Stateright, simulation,
property, and composite entry points also reject the `Unverifiable` IR variant,
so bypassing the cascade cannot turn an omitted safety claim into proof.

## Runtime-enforced claims

The corpus contains non-empty string and counter-to-counter assertions. They
compile into the closed `RuntimeAssert` enum and contract version 2. Production
and deterministic simulation share the evaluator; live actions are checked on
tentative post-transition state before persistence/publication with atomic
rollback, checked counter overflow is rejected, blob-backed oversized strings
retain their logical non-empty meaning, and replay/hydration reject invalid durable
state. Caller payloads are type-checked before internal blob-envelope semantics apply.
A hot swap cannot change an existing entity type's runtime contract until
an explicit migration validates its durable state. No arbitrary string expression
can enter this classification. Absence of either a model encoding or this explicit
contract remains a deployment-blocking error.
