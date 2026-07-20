# ADR-0190: Unsupported safety invariants fail closed

- Status: Proposed
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Supersedes: ADR-0016 Sub-Decision 2 (warning-only handling for unverifiable assertions)
- Related:
  - ADR-0016: Verification Cascade Hardening
  - `crates/temper-spec/src/automaton/assert_parser.rs` (shared typed assertion parser)
  - `crates/temper-verify/src/model/builder.rs` (verification capability classification)
  - `crates/temper-verify/src/cascade.rs` (deployment-facing verification result)
  - `crates/temper-runtime/src/scheduler/sim_handler.rs` (runtime assertion evaluation)
  - `crates/temper-server/src/entity_actor/sim_handler.rs` (actor-simulation adapter)
  - `crates/temper-platform/src/deploy.rs` (cached deployment verification gate)
  - `crates/temper-platform/src/bootstrap.rs` (cached and trusted bootstrap gate)

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

Temper already has one typed assertion parser in `temper-spec`. The missing abstractions
are an explicit capability gate between parsed safety intent and the verification
backends, and a first-class classification for assertions whose safety is guaranteed by
the actor commit boundary rather than by state-space proof.

## Decision

### Sub-Decision 1: Unsupported means verification failure

`InvariantKind::Unverifiable` remains the model-builder representation for an assertion
that the shared `ParsedAssert` grammar or the verifier's typed translation cannot
support. It is a hard verification failure everywhere it can be observed:

- the cascade cannot set `all_passed = true` when any invariant is unverifiable;
- symbolic induction reports the invariant as not proved;
- exhaustive and composite model-check evaluation must not treat it as true;
- simulation and property testing reject unsupported declarations even when their case,
  tick, actor, or seed budget is zero;
- actor assertion extraction retains an explicit unsupported sentinel, and runtime
  evaluation treats that sentinel as a violation if a direct actor-simulation caller
  bypasses the cascade;
- cached verification results and prebuilt trust bundles remain subject to the same
  cheap capability preflight before they can skip the full cascade.

The cascade decides this before relying on state reachability, action generation, seed
selection, or fault scheduling. Unsupported safety intent is a verifier capability
error, not a state-dependent counterexample.

**Why this approach**: A declared safety property has only two truthful successful
outcomes: it was proved by a supported verifier encoding, or it was explicitly assigned
to another tested enforcement mechanism. Anything else blocks deployment.

### Sub-Decision 2: Keep one typed assertion parser

The existing `temper_spec::automaton::ParsedAssert` remains the canonical syntax IR
shared by verification and runtime assertion consumers. This change does not add a
second parser or infer meaning from assertion strings in the cascade. The model builder
continues to translate supported `ParsedAssert` variants into `InvariantKind`; failure to
parse or translate produces `Unverifiable` with the original expression.

**Why this approach**: Parser duplication would allow verification and runtime semantics
to diverge again. Capability rejection belongs after the shared parse and typed
translation, where the verifier can state exactly what it cannot encode.

The existing production spellings `is_true <bool>` and `true` are normalized
within that parser to the already-supported boolean requirement and empty
conjunction IR respectively. They are typed aliases, not capability exemptions.

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

### Sub-Decision 4: Typed runtime-enforced assertions have an atomic contract

The shared parser recognizes two checked-in forms that the bounded model does not encode:
non-empty strings (`field != ''`) and counter-to-counter comparisons. The model builder
classifies these as `RuntimeEnforced(RuntimeAssert)`, distinct from both model-proved
assertions and `Unverifiable`. Each compiled `TransitionTable` carries the typed
`RuntimeInvariant` values and enforcement contract version 2.

The entity handler evaluates these assertions on tentative post-transition state after
effects, status fallback, and action-parameter field synchronization, but before event
construction, persistence, custom-effect publication, or scheduled-action publication.
Initial entity fields, direct field updates, and deletion use the same gate. A failure
restores the complete pre-mutation state and returns an error. The same typed evaluator
is used by production and deterministic simulation. Hydration validates loaded snapshots
and evaluates every replayed event, including tombstones, before making recovered state
available; a violation fails recovery rather than serving invalid durable state.
Declared counter, boolean, and string initial values are compiled into the transition
table so every creation path starts from the same logical state. Oversized non-empty
strings retain their logical meaning when projected as content-addressed blob references.
Checked counter addition rejects numeric overflow and restores the whole pre-action state.
Creation, action, direct-update, and replay payloads are type-checked against declared
state variables before evaluation, so callers cannot impersonate an internal blob
envelope or silently retain a default after submitting an invalid counter. JIT, verifier,
production, and simulation use the same bool/counter initial-value parsers.

A registry hot swap may retain durable snapshots, event tails, projected rows, and live
actors. Therefore a change to an existing entity type's runtime-invariant contract is
rejected atomically until an explicit migration has validated the existing state; the
old CSDL, source, and table remain active. The alternate Postgres actor runtime rejects
runtime-enforced specs at startup until it implements the same atomic contract.

`true` and `is_true <bool>` remain model-proved aliases. Unknown expressions never become
runtime-enforced implicitly: only the closed `RuntimeAssert` enum qualifies, and all
other untranslatable syntax is `Unverifiable`/`TVE001`.
Successful cascade results disclose every runtime-enforced declaration as a non-fatal
warning stating its contract version and that it was not model-proved.

**Why this approach**: These safety claims concern complete entity data that the current
abstract model intentionally does not carry. Enforcing them at the atomic commit boundary
preserves the working applications without pretending the model proved data it omitted.
The typed, versioned contract makes the enforcement owner and lifecycle explicit.

## Rollout Plan

1. Add a behavioral regression showing that an unsupported invariant deterministically
   makes the cascade fail, independent of reachability and simulation seeds.
2. Add structured source diagnostics and the cascade capability gate.
3. Align direct symbolic, exhaustive, and composite evaluation with fail-closed
   semantics.
4. Compile typed runtime assertions into `TransitionTable` and enforce them atomically in
   live action handling, deterministic simulation, full replay, and snapshot hydration.
5. Invalidate old cached/trusted false passes with a source capability preflight before
   every deployment or bootstrap cache decision.
6. Reject runtime-contract hot swaps without an explicit durable-state validation and
   migration, leaving the previous registry configuration untouched.
7. Run the checked-in IOA corpus and prove every declaration is either model-proved or
   attached to runtime enforcement contract version 2; do not weaken or delete claims.

## Readiness Gates

- No cascade containing `InvariantKind::Unverifiable` reports `all_passed = true`.
- Diagnostics include the invariant name, assertion text, and exact source range.
- Every model-proved invariant form retains its existing verification behavior.
- Runtime-enforced declarations reject invalid live actions and invalid durable replay.
- All 120 checked-in declarations have a typed model or runtime-enforced classification.
- Unchanged cached specs and trusted bundles cannot bypass the capability gate.
- Cached results retain the disclosure that runtime-enforced claims were not model-proved.
- Existing entity types cannot activate a changed runtime contract without durable-state
  validation and migration.
- Direct backend entry points reject unsupported declarations at zero exploration budget.
- Actor simulation reports a truly unsupported sentinel as an invariant violation, while
  runtime-enforced forms execute the same evaluator and handler path as production.

## Consequences

### Positive

- Verification success once again means every declared safety invariant was understood
  by the verifier.
- Failure no longer depends on whether randomized or exhaustive execution reaches an
  invariant's trigger state.
- Tooling receives stable, source-addressable diagnostics.
- Parser, runtime, simulation, and replay share one typed assertion grammar and evaluator.
- Verification cache durability no longer preserves pass artifacts issued under the old
  warning-only capability policy.

### Negative

- Existing specifications with truly unknown assertions stop deploying until a typed
  model-proof or runtime-enforcement encoding is added.
- `CascadeResult` gains a structured diagnostic field that serializers and UIs may
  choose to display.
- Public assertion/model structures and the runtime `SpecAssert` enum gain source or
  enforcement fields/variants. This is a deliberate source-level API change for
  exhaustive downstream struct literals and matches. Serialized transition tables that
  predate the runtime-contract field are rejected and must be rebuilt from source.
- Runtime enforcement adds one deterministic check. Action execution retains a
  pre-action state snapshot so both safety rejection and checked-arithmetic failure can
  roll back every tentative effect.

### Risks

- A production spec may have relied on truly unknown false-success behavior. This is an
  intentional compatibility break: retaining deployment for an unchecked safety claim
  would preserve the defect.
- Source-location extraction could drift from the hand-rolled automaton parser.
  Diagnostics therefore use TOML array-table spans from the same submitted document and
  are covered by double-quoted, single-quoted, CRLF, inline-comment, and repeated-invariant
  tests. A missing source span is returned as an error by the public diagnostic API rather
  than panicking.
- Old persisted verification rows do not carry a capability-policy version. Deployment
  and bootstrap therefore rerun the cheap typed capability preflight before consulting
  hash caches or bundle trust; only supported specs retain the performance benefit.
- A future runtime assertion semantic change could invalidate compiled artifacts. Every
  runtime assertion therefore carries enforcement version 2, deployment/bootstrap
  rebuild tables from source after capability preflight, and deserialization rejects
  pre-contract tables that lack the runtime-invariant field.
- Runtime contracts cannot be introduced or changed through ordinary hot swap for an
  existing entity type. Operators must run an explicit state-validation migration first;
  this is intentionally conservative because the synchronous registry swap has no safe
  way to enumerate every durable backend and live actor atomically.

### DST Compliance

This change touches simulation-visible `temper-runtime`, `temper-jit`, and `temper-server`
code. Runtime adds explicit `RuntimeEnforced` and `Unsupported` assertion variants. The
former is enforced inside the same real entity handler used by production; the latter is
a pure deterministic failure. Fixed seed 213 exercises atomic rejection and rollback for
string and counter assertions, counter overflow, initial-state validation, and deletion.
Replay injects invalid snapshots, event tails, and tombstones and proves recovery fails.
Existing fault-scheduled simulation covers the unsupported sentinel. The change
adds no clock, ambient I/O, thread, or unordered collection, and the capability gate and
runtime evaluator are pure deterministic functions. The DST reviewer must pass the final
diff before commit.

## Non-Goals

- Rewriting checked-in application safety declarations to narrower assertions.
- Treating arbitrary unparsed expressions as runtime-enforced.
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
5. **Rewrite application assertions into model-supported approximations** — Rejected
   because it would weaken working safety claims and hide the data-model gap.

## Rollback Policy

The diagnostic representation can be revised additively, but unsupported safety
assertions must not return to warning-only behavior. If rollout exposes a required
assertion form, add a typed parser/verification encoding or an explicitly governed
runtime-only contract; do not restore false-success compatibility.
