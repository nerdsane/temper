# ADR-0045: Reactions as a First-Class App Primitive

- Status: Proposed
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0015: Agent OS Cross-Entity Primitives (cross-entity guards at the action layer)
  - [nerdsane/temper#128](https://github.com/nerdsane/temper/issues/128)
  - `crates/temper-server/src/reaction/` (ReactionDispatcher, registry, resolver, types)
  - `crates/temper-server/src/state/dispatch/cross_entity.rs` (cross-entity guard fetch helper to reuse)
  - `os-apps/temper-fs/reactions/reactions.toml` (only production consumer of hand-authored reactions today)

## Context

Temper already ships a production-grade cross-entity reaction system. `ReactionDispatcher` fires after every successful action, with deterministic ordering, tenant isolation, fire-and-forget semantics, and a bounded cascade depth (`MAX_REACTION_DEPTH = 8`). It works, and `paw-fs` uses it correctly.

Above the surface it is almost invisible. `paw-fs` is the only app with a hand-authored `reactions.toml`. Every other app that needs a cross-entity transition writes a WASM module whose sole job is to call `temper_action()` on another entity — WASM-as-plumbing. The motivating example is katagami-curation's `build_session_message` WASM, which exists solely to react to `CurationJob.Submit` by creating a Session and dispatching `Configure` on it. This is exactly what reactions already solve for paw-fs.

Three concrete capabilities are missing that would let reactions replace that WASM, plus a documentation gap that stops developers from finding the primitive in the first place:

1. **Dynamic param mapping** — today `ReactionTarget.params` is wired through the dispatcher (`dispatcher.rs:113`) but only accepts static TOML literals. There's no way to pipe source-entity field values into the target action's params.
2. **Conditional guards on reactions** — today `ReactionTrigger` matches on `(entity_type, action, to_state)`. Branching pipelines ("only fire when `job_type == 'source_search'`") require either a second-entity indirection or WASM.
3. **Fresh-ID create resolver** — `CreateIfMissing` requires a derivable ID on the source entity. Pipeline chaining ("each CurationJob.Complete creates a new CurationJob") needs a genuinely new UUID.
4. **Documentation** — `AGENTS.md` currently tells developers "if logic runs on state change, write a WASM integration." That advice predates reactions being usable for general pipelines.

Correction on the issue's framing: `ReactionTarget.params` is *fully wired today*. The gap is not threading, it is that params are static-only. `params_from` closes that specific gap.

## Decision

Extend the reaction system along four axes. All additions are additive with `#[serde(default)]`; existing `paw-fs` reactions and `[[agent_trigger]]`-synthesized rules parse and behave byte-identically.

### Sub-Decision 1: `params_from` — dynamic params from source fields

Add `params_from: BTreeMap<String, String>` to `ReactionTarget`. Keys name target-action params; values name fields on the source entity. At dispatch time, a shared helper `build_effective_params` merges the static `params` with values read out of the source entity's `fields` value. Collision between `params` and `params_from` keys is a parse-time error. Missing source field at runtime → `tracing::warn!` and skip the key; the reaction still fires with a partial param map (consistent with existing resolver `None`-on-missing posture).

```toml
[reaction.then]
entity_type = "CurationJob"
action      = "Submit"
params      = { requested_by = "system" }
params_from = { job_type = "next_stage", input = "output" }
```

**Why this shape**: the issue's TOML sketch. BTreeMap (not HashMap) for DST compliance. Static + dynamic coexist so literals can be mixed with piped values without introducing a template language. A single helper serves both `dispatcher` and `sim_dispatcher` so production and simulation stay in lockstep.

### Sub-Decision 2: `Create` target resolver — fresh UUID

Add a variant to `TargetResolver` that returns a genuinely new entity ID on every dispatch via `sim_uuid()`. Distinct from `CreateIfMissing` (which is keyed on an `id_field` and intended for per-source-entity singletons like "one FileVersion per File commit").

```toml
[reaction.resolve_target]
type = "create"
```

**Why `sim_uuid`, not `Uuid::new_v4`**: `reaction` lives in `temper-server`, a simulation-visible crate. DST rules (temper CLAUDE.md § Deterministic Simulation) require seeded sources. `sim_uuid()` already exists in `temper-runtime` and is what the rest of the reaction path uses; reusing it keeps `SimReactionSystem` deterministic across seeded runs.

**Compositional value with Sub-Decision 1**: `Create` + `params_from` together replace the katagami `build_session_message` WASM exactly — fresh Session ID with fields piped from the source CurationJob, declared in TOML with zero code.

### Sub-Decision 3: `ReactionGuard` — conditional firing (source + cross-entity)

Add `guard: Option<ReactionGuard>` to `ReactionTrigger`. `ReactionGuard` is a new enum with three families:

- **Sync source-only (no I/O):** `field_equals { field, value }`, `field_in { field, values }`, `bool_true { field }`, `bool_false { field }`, `state_in { values }` — evaluate against the source entity's post-action fields already in hand.
- **Async cross-entity:** `cross_entity_state_in { entity_type, entity_id_source, required_status }` — fetch another entity's current state and compare. Deliberately mirrors the IOA guard variant of the same name (ADR-0015) so developers transfer knowledge between the two evaluation paths. The implementation reuses the existing fetch helper inside `crates/temper-server/src/state/dispatch/cross_entity.rs`; we do not duplicate the fetch logic.
- **Composite:** `all_of`, `any_of`, `not` — bounded at `MAX_GUARD_DEPTH = 4`, validated at parse time.

Guard-skipped rules do *not* emit a `ReactionResult`. They never fired. This matters for the `last_results()` contract and for tests that count results.

```toml
[reaction.when]
entity_type = "CurationJob"
action      = "Complete"
[reaction.when.guard]
type  = "field_equals"
field = "job_type"
value = "source_search"
```

**Why a new enum rather than reusing `temper-jit::Guard`**: IOA guards are compiled into the transition table and evaluated *pre-action* with transition-table context. Reaction guards evaluate *post-action* against the source entity's resulting fields, which are already available for sync variants. Reusing the full IOA enum would drag in transition-table machinery we don't need. Mirroring variant *names* and TOML vocabulary keeps the developer experience consistent without coupling the two evaluation paths.

**Why include cross-entity guards now, not later**: the motivating apps (katagami pipelines, session-completion callbacks on a parent Workspace) will want them soon. Adding them to the enum in a follow-up would change the shape (adding async to `evaluate`) and force a second parser/test pass. Cheaper to ship once.

### Sub-Decision 4: Documentation — make reactions discoverable

- New `docs/reactions.md`: what reactions are, when to use them vs. WASM, full TOML reference, three example patterns (pipeline chaining, session-completion callback, cleanup-on-failed), and the invariants that don't change (fire-and-forget, `MAX_REACTION_DEPTH = 8`, determinism).
- Amend `AGENTS.md`: replace "if logic runs on state change, it's a WASM integration" with "if a state change needs to trigger another entity's action, use a reaction (`docs/reactions.md`). If it needs computation, external I/O, or a new WASM-authored field, use an integration."

**Why this matters for architecture, not just ergonomics**: undocumented primitives accumulate WASM workarounds. Every such workaround puts cross-entity wiring inside a WASM module's return value, which is opaque to traces, opaque to Cedar, and untestable without hosting the module. Documenting reactions closes that leak.

### Sub-Decision 5: Keep reactions separate from actions (architectural reaffirmation)

We considered folding cross-entity transitions into the action layer itself (e.g., letting an action's TOML declare downstream entity transitions inline, eliminating the reaction primitive). Rejected. The separation is load-bearing:

1. **Verification tractability.** `temper-verify` proves each action preserves *its own entity's* invariants because each entity is a closed I/O automaton. Cross-entity transitions inside actions would force joint invariants across every pair of entities that could interact — combinatorial explosion for the model checker.
2. **Authorization scope.** Actions run under the invoking principal's Cedar context. Reactions elevate to `AgentContext::system()` at `dispatcher.rs:114` — the platform enacts downstream transitions, not the user. Fusing the layers means either users inherit cross-entity authority they shouldn't, or we reinvent reaction-style elevation inside the action path.
3. **Transactional boundary.** Actions are all-or-nothing with respect to their entity's invariants. Reactions are fire-and-forget — a failing reaction doesn't roll back the source. Different failure semantics; fusing them forces one or the other.
4. **Bounded cascades.** `MAX_REACTION_DEPTH = 8` lives in the reaction dispatcher. Actions-transitioning-actions has no natural cap; you'd need the same bound in a different place.
5. **Composition vs. contract.** Actions are the entity's verified contract. Reactions are how apps *compose* entities. Mixing them forces every cross-entity wiring change to re-verify the contract surface.
6. **Standard pattern.** Pure transition + declarative effect is the split in Elm, Redux middleware, Kafka Streams, FoundationDB observers. Temper follows that pattern; we're not inventing one.

This sub-decision doesn't ship code. It's a commitment: Track 4 extends reactions; it does not fuse them into actions.

## Rollout Plan

One PR on `nerdsane/temper` from `feat/reactions-first-class-primitive`. Phase commits are atomic inside the branch so reviewers can walk the history: ADR → types → `params_from` → `Create` resolver → guards → docs.

1. **Commit 1** — this ADR.
2. **Commit 2** — `ReactionTarget.params_from` + `build_effective_params` helper + registry/dispatcher/sim_dispatcher wiring + unit/integration tests.
3. **Commit 3** — `TargetResolver::Create` variant + `sim_uuid` branch in resolver + registry parse + tests (including DST determinism).
4. **Commit 4** — `ReactionGuard` enum + `guard.rs` evaluator (sync + async-cross-entity reusing `cross_entity.rs` fetch) + registry parse with `MAX_GUARD_DEPTH` + dispatcher/sim_dispatcher integration + tests.
5. **Commit 5** — `docs/reactions.md` + `AGENTS.md` amendment.

All five land in one PR.

## Readiness Gates

- `cargo test --workspace` green, including `temper-platform --test platform_e2e_dst`.
- `paw-fs` regression: existing `os-apps/temper-fs/reactions/reactions.toml` loads and dispatches byte-identically under the extended parser.
- Agent-trigger synthesis regression: `synthesize_agent_trigger_reactions` output unchanged (no `guard`, no `params_from`, no `Create`).
- DST replay: two seeded runs under `SimReactionSystem` produce identical reaction firing order and identical `Create`-resolver IDs.
- DST reviewer + code-reviewer PASS markers present before commit.
- Pre-push 4-gate pipeline green (rustfmt, clippy, readability ratchet, full test suite).

## Consequences

### Positive
- Cross-entity wiring moves from WASM modules into TOML, becoming visible to traces, reviewable as code, and testable without WASM hosting.
- Pipeline chaining (per-stage CurationJob fan-out) becomes a 6-line TOML snippet.
- Session-completion callbacks and cleanup-on-failed flows — long-standing WASM workarounds — become declarative.
- Reaction observability improves: each reaction has a name, rule table entry, and tracing span.

### Negative
- `reaction.rs` surface area grows (new enum, new module, additional helper). Mitigated by keeping the action layer untouched; all complexity lives inside `crates/temper-server/src/reaction/`.
- Cross-entity guards add async fetches to the reaction path. Capped by `MAX_REACTION_DEPTH = 8`; a per-dispatch counter attaches to the tracing span so cost stays visible.

### Risks
- **Drift between sync prod path and sim path.** Sync `dispatcher` and `sim_dispatcher` both implement guard + merged-params logic. Mitigation: extract `build_effective_params` and guard evaluation into shared helpers; integration tests run both paths on the same scenarios.
- **Over-eager developer use replacing WASM that *should* stay WASM.** Reactions are fire-and-forget and capped at depth 8; they are not a substitute for computation-heavy or I/O-bearing logic. Mitigation: `docs/reactions.md` explicitly enumerates the "still use WASM" cases (external I/O, computation, >8 cascade depth).
- **Cross-entity guard latency in deep cascades.** Each guard costs one entity fetch. Mitigation: fetch counter in trace span + existing depth budget + readiness-gate review of cross-entity guard use in new apps.

### DST Compliance
- `TargetResolver::Create` uses `temper-runtime::sim_uuid()`, not `uuid::Uuid::new_v4`.
- All new collections are `BTreeMap` / `BTreeSet` for deterministic iteration.
- Guard evaluation is a pure function of `(guard, source_fields)` for sync variants; for `cross_entity_state_in`, the sim path reads from the in-memory entity store synchronously (no wall-clock waits).
- No new `static mut`, `lazy_static!`, or `thread_local!`.
- No new `chrono::Utc::now()` or `std::thread::sleep()`.

## Non-Goals

- **No change to fire-and-forget semantics.** Reactions remain non-transactional.
- **No expression DSL.** The issue's `"job_type eq 'source_search'"` syntax is rendered as structured enum TOML. Adding a parser is a larger commitment than this track scopes.
- **No change to `MAX_REACTION_DEPTH` (8) or `MAX_REACTIONS_PER_TENANT` (256).**
- **No fusion with the action layer.** See Sub-Decision 5.
- **No changes to `paw-fs` reactions or `[[agent_trigger]]` synthesis.** Both remain byte-identical.
- **No katagami-curation rewrite in this track.** Katagami consumes the new primitives after this PR merges; that rewrite is a separate track in katagami's repo.

## Alternatives Considered

1. **Expression DSL for guards** (e.g., `"job_type eq 'source_search'"`). Rejected: Temper's codebase uses structured Rust enums for guards, not expressions. A DSL would need a parser, its own error-reporting story, and would diverge from IOA guard ergonomics. Structured TOML keeps the two paths consistent.
2. **Fold cross-entity transitions into the action layer.** Rejected (Sub-Decision 5): breaks verification tractability, authorization boundary, transactional semantics, and cascade bounds.
3. **Source-entity-only guards now; cross-entity guards as a follow-up.** Rejected: the follow-up would change `ReactionGuard::evaluate` from sync to async, forcing a second pass through the parser and every call site. Cheaper to ship once.
4. **Strict miss-on-`params_from` (fail the whole reaction when a source field is missing).** Rejected: inconsistent with existing resolver `None` posture. Partial params + warn matches how `resolver::Field` already handles drift.
5. **Unbounded composite guards.** Rejected: cascade depth already caps reaction fan-out; unbounded guard nesting would make evaluation cost unpredictable. `MAX_GUARD_DEPTH = 4` is TigerStyle — bound budgets rather than hope.

## Rollback Policy

All additions are additive with serde defaults. Reverting the PR restores the previous reaction surface exactly; `paw-fs` and agent-trigger synthesis are unaffected by design.

If a specific sub-decision proves wrong after merge:
- `params_from`: remove the field from `ReactionTarget` and the `build_effective_params` helper; existing TOML without `params_from` is unaffected.
- `Create` resolver: remove the variant; apps using it must switch to `CreateIfMissing` with an explicit `id_field`.
- Guards: remove the field from `ReactionTrigger`; guarded rules become unconditional (review carefully before pulling — this could fire reactions that were previously gated off).
