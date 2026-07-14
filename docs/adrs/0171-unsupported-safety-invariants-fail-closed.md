# ADR-0171: Unsupported safety invariants fail closed

- Status: Proposed
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Supersedes: ADR-0016 Sub-Decision 2 (warning-only handling for unverifiable assertions)
- Related:
  - ADR-0016: Verification Cascade Hardening
  - `crates/temper-spec/src/automaton/assert_parser.rs` (shared typed assertion parser)
  - `crates/temper-verify/src/model/builder.rs` (verification capability classification)
  - `crates/temper-verify/src/cascade.rs` (deployment-facing verification result)

## Context

ADR-0016 replaced an implicit, vacuous invariant fallback with an explicit
`InvariantKind::Unverifiable`, but deliberately made that classification a warning. The
verification backends all permit it through different mechanisms: symbolic and
exhaustive model checking treat it as true or omit it, model-level simulation and
property testing return `false` from their "is violated" predicates, and actor simulation
filters unsupported forms out of its runtime assertion set. Consequently, every level
can report success even though the verifier knows before exploration that it cannot
prove a declared safety property.

This behavior is present in checked-in application specifications. For example,
`os-apps/temper-fs/specs/workspace.ioa.toml` compares one counter to another
(`used_bytes <= quota_limit`), while the shared typed assertion parser currently supports
counter-to-literal comparisons only. A cascade can therefore report success without
checking the declaration that consumers believe was model-checked.

Temper already has one typed assertion parser in `temper-spec`. The missing abstraction
is not a second expression language; it is an explicit capability gate between parsed
safety intent and the verification backends.

## Decision

### Sub-Decision 1: Unsupported means verification failure

`InvariantKind::Unverifiable` remains the model-builder representation for an assertion
that the shared `ParsedAssert` grammar or the verifier's typed translation cannot
support. It is a hard verification failure everywhere it can be observed:

- the cascade cannot set `all_passed = true` when any invariant is unverifiable;
- symbolic induction reports the invariant as not proved;
- exhaustive and composite model-check evaluation must not treat it as true;
- simulation, property testing, and actor assertion extraction never receive an
  unsupported declaration as executable safety logic because capability validation has
  already rejected the cascade.

The cascade decides this before relying on state reachability, action generation, seed
selection, or fault scheduling. Unsupported safety intent is a verifier capability
error, not a state-dependent counterexample.

**Why this approach**: A declared safety property has only two truthful successful
outcomes: it was proved by a supported verifier encoding, or it was explicitly assigned
to another tested enforcement mechanism. Temper has no runtime-only classification with
an enforcement proof today, so every unsupported declaration must block deployment.

### Sub-Decision 2: Keep one typed assertion parser

The existing `temper_spec::automaton::ParsedAssert` remains the canonical syntax IR
shared by verification and runtime assertion consumers. This change does not add a
second parser or infer meaning from assertion strings in the cascade. The model builder
continues to translate supported `ParsedAssert` variants into `InvariantKind`; failure to
parse or translate produces `Unverifiable` with the original expression.

**Why this approach**: Parser duplication would allow verification and runtime semantics
to diverge again. Capability rejection belongs after the shared parse and typed
translation, where the verifier can state exactly what it cannot encode.

### Sub-Decision 3: Structured diagnostics identify the declaration

`CascadeResult` exposes structured unsupported-invariant diagnostics in addition to its
human-readable summary. Each diagnostic contains a stable error code, invariant name,
original assertion, and the source range in the submitted IOA document as byte offsets
and one-based line/column positions. Source ranges are derived from the original TOML
document; they are not reconstructed from normalized model strings.

Warnings remain available for non-fatal advisory information, but unsupported safety
assertions are errors and are not described as skipped warnings.

**Why this approach**: Callers need a machine-readable deployment gate, while developers
need to locate the exact declaration without searching a generated specification.

### Sub-Decision 4: Runtime-only safety requires a future explicit contract

No assertion is implicitly downgraded to runtime-only. Introducing that classification
requires a separate ADR defining the enforcement owner, proof artifact, deployment gate,
and tests demonstrating that every transition path invokes the runtime check.

**Why this approach**: A label without a verified enforcement path would recreate the
same false-success behavior under a different name.

## Rollout Plan

1. Add a behavioral regression showing that an unsupported invariant deterministically
   makes the cascade fail, independent of reachability and simulation seeds.
2. Add structured source diagnostics and the cascade capability gate.
3. Align direct symbolic, exhaustive, and composite evaluation with fail-closed
   semantics.
4. Run the checked-in IOA corpus and report unsupported declarations for remediation;
   do not weaken or delete those declarations to make the corpus pass.

## Readiness Gates

- No cascade containing `InvariantKind::Unverifiable` reports `all_passed = true`.
- Diagnostics include the invariant name, assertion text, and exact source range.
- Every supported invariant form retains its existing verification behavior.
- Checked-in unsupported declarations are reported deterministically before deployment.

## Consequences

### Positive

- Verification success once again means every declared safety invariant was understood
  by the verifier.
- Failure no longer depends on whether randomized or exhaustive execution reaches an
  invariant's trigger state.
- Tooling receives stable, source-addressable diagnostics.
- Parser and runtime consumers continue to share one typed assertion grammar.

### Negative

- Existing specifications with unsupported assertions will stop deploying until the
  verifier gains the required typed encoding or the specification is corrected.
- `CascadeResult` gains a structured diagnostic field that serializers and UIs may
  choose to display.

### Risks

- A production spec may have relied on the previous false-success behavior. This is an
  intentional compatibility break: retaining deployment for an unchecked safety claim
  would preserve the defect.
- Source-location extraction could drift from the hand-rolled automaton parser.
  Diagnostics therefore use TOML array-table spans from the same submitted document and
  are covered by multiline and repeated-invariant tests.

### DST Compliance

This change is confined to `temper-spec` and `temper-verify`; it does not touch the
simulation-visible runtime, JIT, or server crates. Capability validation is a pure,
deterministic function of the submitted IOA bytes. No clock, randomness, ambient I/O, or
unordered collection is introduced.

## Non-Goals

- Adding counter-to-counter comparison support in this change.
- Rewriting checked-in application safety declarations to narrower assertions.
- Defining a runtime-only invariant classification without an enforcement proof.
- Changing action, guard, liveness, or field-invariant grammar.

## Alternatives Considered

1. **Keep warnings and require callers to inspect them** — Rejected because existing
   deployment callers correctly use `all_passed` as the gate; advisory text must not
   override a successful machine-readable result.
2. **Rely on simulation/property exploration to expose the gap** — Rejected because
   verifier capability is known before exploration, while the existing unsupported-kind
   violation predicates report no violation. State reachability and seeds cannot turn an
   unsupported declaration into a proved safety property.
3. **Reject unsupported assertions in the general IOA parser** — Rejected because parsing
   and verifier capability are distinct. Runtime consumers may understand a typed form
   before every verification backend can prove it; the verification boundary must make
   that distinction explicit.
4. **Add a new invariant expression parser in `temper-verify`** — Rejected because it
   would duplicate `ParsedAssert` and invite semantic drift.

## Rollback Policy

The diagnostic representation can be revised additively, but unsupported safety
assertions must not return to warning-only behavior. If rollout exposes a required
assertion form, add a typed parser/verification encoding or an explicitly governed
runtime-only contract; do not restore false-success compatibility.
