# ADR-0046: Unified Action Triggers — Supersede Reactions Subsystem

- Status: Proposed
- Date: 2026-04-20
- Deciders: Temper core maintainers
- Supersedes: ADR-0045 (specifically its Sub-Decision 5 — "Keep reactions separate from actions")
- Related:
  - ADR-0015: Agent OS Cross-Entity Primitives (cross-entity guards at the action layer)
  - ADR-0045: Reactions as a First-Class App Primitive (superseded by this ADR in its Sub-Decision 5)
  - `crates/temper-server/src/reaction/` (to be retired)
  - `crates/temper-spec/src/automaton/types.rs` (`[[action.triggers]]` lives here)
  - `crates/temper-verify/src/cascade.rs` (joint verification extension)
  - `crates/temper-authz/src/engine/mod.rs` (parse-time authority check + `is_system` bypass removal)
  - `crates/temper-spec/src/cross_invariant/` (integrates into cascade)
  - `os-apps/temper-fs/reactions/reactions.toml` (sole production reactions file — migrates inline)

## Context

ADR-0045 established reactions as a separate primitive with their own TOML file (`reactions.toml`) and dispatcher (`ReactionDispatcher`), justified by verification tractability, authorization scope, transactional boundary, and bounded cascades. After implementation, four structural problems surfaced:

1. **Discoverability failure.** Entity specs contain no hint of outgoing reactions. Developers consistently bypassed reactions in favor of WASM-as-plumbing, which IS visible on the source action (via `effect = "trigger <name>"`). Only `os-apps/temper-fs/reactions/reactions.toml` exists in the whole codebase. Every other app that needs cross-entity wiring writes a WASM module whose sole job is to call `temper_action()` on another entity — the pattern ADR-0045 was supposed to eliminate.

2. **Installation and recovery silently drop reactions.** Four bug sites:
   - `crates/temper-platform/src/os_apps/mod.rs:1128` — `AppBundle` has no `reactions` field; `load_app_bundle` never reads `{app_dir}/reactions/reactions.toml`.
   - `crates/temper-platform/src/bootstrap.rs:175` — `bootstrap_tenant_specs` hardcodes `Vec::new()` for reactions.
   - `crates/temper-server/src/registry_bootstrap.rs:167, 361` — both recovery paths (Postgres/Turso restore, platform-store restore) pass `Vec::new()` for reactions.
   - Reactions are never persisted to any durable store; on Railway restart, they vanish until `install_os_app` re-runs — which itself doesn't load them.

3. **Authorization is an unconditional bypass.** Every reaction runs as `AgentContext::system()` (`crates/temper-server/src/reaction/dispatcher.rs:144`), which triggers `is_system → Allow` in `crates/temper-authz/src/engine/mod.rs:494-530`. Cedar is never consulted for reaction-fired actions. Any public source action wired to a sensitive target becomes a silent privilege escalator. Developers have no mechanism to scope a reaction's authority.

4. **Verification is snapshot-only.** Cross-invariants (`cross-invariants.toml`) check static joint states; they are NOT fed into `temper-verify`. Reactions are treated as "permissive/always-true" in the model checker — `crates/temper-verify/src/model/builder.rs:114-122` explicitly converts `CrossEntityState` guards to `Always` because "cross-entity guards are runtime-only." Intermediate states during a cascade are unverified. Liveness properties (`eventually target_action fires`) are not expressible.

ADR-0045's arguments for separation do not hold up under scrutiny:

- **Authorization scope** is solvable by explicit `principal` per trigger with Cedar attribute population, not by file location.
- **Transactional boundary** (fire-and-forget) is preserved whether the trigger is inline on the action or in a separate file.
- **Verification tractability**: joint verification of realistic trigger chains (2–3 entities, ~5 states each) is ~125 composed states — trivially tractable for Stateright BFS. The "cubic blowup" argument was a worst-case straw man; real reaction graphs are small.
- **Composition vs. contract**: tenants customize apps at install time anyway (`registry/mod.rs` supports `merge = true`). The distinction between "entity-owned" and "app-orchestration" reactions is where defaults ship, not a semantic invariant.

The discoverability cost is the dominant factor and outweighs the cleanliness of separation. Reactions must live inside the entity spec so they are impossible to miss.

## Decision

### Sub-Decision 1: Cross-entity wiring moves inline as `[[action.triggers]]`

Each `[[action]]` in an IOA spec gains a repeated nested `[[action.triggers]]` block. Readers of an entity spec see the complete outgoing surface of every action. One location per source entity; no separate `reactions.toml`.

```toml
[[action]]
name = "StreamUpdated"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type"]

[[action.triggers]]
name = "stream_updated_creates_version"
kind = "entity"
principal = "file-service"
target_entity = "FileVersion"
target_action = "Create"
resolve_target = { type = "create_if_missing", id_field = "last_version_id" }
```

**Why this shape**: an action's outgoing effects are part of its contract. Making them visible in the entity spec aligns with how developers read entities (top-to-bottom, action-by-action). The nesting under `[[action]]` makes the relationship unambiguous — a trigger always belongs to its source action.

### Sub-Decision 2: `[[action.triggers]]` unifies three trigger kinds via `kind` discriminator

One primitive covers all outgoing effects. Deletes `[[integration]]` and `reactions.toml` entirely.

- `kind = "entity"` — cross-entity action dispatch (former reactions).
- `kind = "wasm"` — WASM module execution (former `[[integration]] type = "wasm"`).
- `kind = "webhook"` — outbound HTTP (former `[[integration]] type = "webhook"`).

Kind-specific fields are validated at parse time: `Entity` requires `target_entity` + `target_action`; `Wasm` requires `module`; `Webhook` requires `url` + `method`. `on_success` / `on_failure` apply to `Wasm` and `Webhook` kinds and name entity actions on the source entity to dispatch after module/HTTP execution.

**Why this shape**: today WASM integrations, webhooks, and reactions are three parallel systems with overlapping purposes (all are "things that happen when an action commits"). Unifying them under one schema with a kind discriminator eliminates duplication, makes the full outgoing surface visible in one place, and lets common machinery (guards, `to_state` filter, principal resolution, liveness annotations) apply uniformly.

### Sub-Decision 3: `principal` is optional; defaults to the invoking principal

When a trigger fires without `principal`, the Cedar check for the target action runs with the same `SecurityContext` that invoked the source action. If that principal has authority, the trigger dispatches; if not, the trigger fails (fire-and-forget — the source commit is not rolled back).

Explicit `principal = "<service-name>"` elevates. The name must match a registered `AgentType` entity in the tenant. At dispatch time, a synthetic `SecurityContext` is built with:

```
Principal {
    id:                 "service:<service-name>",
    kind:               Agent,
    role:               "<service-name>",
    agent_type:         "<service-name>",
    agentTypeVerified:  true,
    attributes:         { dispatched_by_trigger: true, source_entity, source_action }
}
```

All relevant Cedar attributes are populated identically, so tenant policies can match whichever style the developer prefers (`principal.role`, `principal.agent_type`, `principal.id`). There is no loophole: the invoking principal cannot choose its own elevation — elevations are declared by the developer in the spec, one per trigger.

**Why this shape**: most triggers genuinely don't need elevation (audit logs, derived denormalizations — the invoking principal can authorize them). Making `principal` optional keeps those cases simple. Elevation is the exception, and when present, it is explicit and reviewable. Populating every Cedar attribute from one name means the developer decides their policy style without the trigger forcing a specific Cedar attribute to match.

### Sub-Decision 4: Delete the `AgentContext::system()` Cedar bypass

`is_system → Allow` (`engine.rs:494-530`) is removed. System principals must be granted explicit Cedar policies like any other agent type. A default `system-platform` AgentType with a narrow Cedar policy package is installed during tenant bootstrap, covering genuinely platform-initiated actions (bootstrap writes, credential rotation, recovery-path writes). Every `AgentContext::system()` caller in the codebase is audited: either replaced with an explicit named principal, or retained and authorized through real Cedar against the `system-platform` policies.

**Why this removal**: the bypass is a blanket privilege grant that does not reflect actual authority boundaries. Any caller that uses it gains full authority over every action on every entity, with no policy gate. Production safety requires the system principal to be a real Cedar principal subject to the same enforcement as any agent.

### Sub-Decision 5: `temper-verify` composes triggered entities into joint state machines

New module `crates/temper-verify/src/composite/`. `CompositeTemperModel` implements Stateright's `Model` trait:

- **State**: `BTreeMap<EntityTypeName, TemperModelState>` (product of participating entities).
- **Action**: `(EntityTypeName, TemperModelAction)` tagged by actor entity.
- **init_states**: cross-product of each entity's initial states.
- **actions**: for each entity in the composition, yield its enabled actions per its local `TemperModel`.
- **next_state**: apply the action to the actor's local state; walk the actor entity's `[[action.triggers]]` for the fired action; for each trigger whose guard passes, apply the target action to the target entity's local state (deterministic in verification; symbolic IDs for `Field`/`Create` resolvers). Recurse bounded by `MAX_TRIGGER_DEPTH = 8`.
- **properties**: union of each entity's local invariants as `Property::always` + translated cross-invariants (joint predicates over product state) + optional `Property::eventually` for triggers declaring `liveness = "required"`.

Composition scope is determined by the reachability set of the trigger graph rooted at the entity being verified. Entities with no incoming or outgoing triggers verify in isolation (fast path, single-entity model unchanged).

`kind = "wasm"` and `kind = "webhook"` triggers are modeled symbolically during verification — the WASM execution / HTTP call is opaque — but their `on_success` / `on_failure` dispatches participate in joint verification like any other entity-kind trigger.

**Why this shape**: Stateright's `Model` trait is pluggable; composition is a wrapper, not a rewrite. The BFS checker (`checker.rs:43-46`) is model-agnostic. Reuse of `TemperModel` as the building block keeps the single-entity fast path intact and scopes the new complexity to `composite/`. Symbolic handling of external I/O keeps verification tractable without pretending to verify opaque module behavior.

### Sub-Decision 6: Cross-invariants integrate into the cascade

Hard-kind cross-invariants (`cross-invariants.toml` with `kind = "hard"`) become joint properties on `CompositeTemperModel` and are proven by the composite L1 model check. Eventual-kind cross-invariants retain their runtime `EventualInvariantTracker` (they require temporal convergence, not just state reachability). The `cross-invariants.toml` per-tenant file remains as the declaration location; the verifier consumes it where today it doesn't.

**Why this shape**: the `related(Entity, field).Status in [...]` assertion grammar is already structured; it translates cleanly to a joint predicate `composite_state[target].status ∈ values`. No new DSL. The eventual kind genuinely needs temporal reasoning that exceeds the scope of snapshot model checking, so it stays runtime.

### Sub-Decision 7: Delete legacy `[[agent_trigger]]`

Agent spawning becomes an explicit `[[action.triggers]]` block with `kind = "entity"` targeting the `Agent` entity (with `principal = "agent-supervisor"` for elevation). The `synthesize_agent_trigger_reactions` function in `registry/relations.rs:91-148` is deleted. Apps using `[[agent_trigger]]` today (`plan.ioa.toml`, `task.ioa.toml`) are migrated in the same PR.

**Why this removal**: `[[agent_trigger]]` is a specialized case of a general cross-entity trigger. Folding it into `[[action.triggers]]` eliminates a parallel primitive and the synthesis layer that bridges them.

### Sub-Decision 8: Minimal liveness hook

`ActionTrigger` carries `liveness: TriggerLiveness` with variants `None | BestEffort | Required`, defaulting to `BestEffort`. When `Required`, the composite verifier adds a `Property::eventually` assertion that the target action (entity-kind) or on_success action (wasm/webhook-kind) fires following the source action.

Fairness assumptions are weakly fair for the dispatcher (implicit). No window bounds, no expression DSL, no `within_ms` annotations — those are deferred to a future ADR if apps request them.

**Why this minimum**: the user directive (this conversation, 2026-04-20) was "don't defer anything — everything is priority." Full temporal-logic expressiveness would add a substantial surface to the spec and verifier for speculative value. The `Required | BestEffort | None` trichotomy covers the pragmatic cases (audit logs must eventually fire; notifications are best-effort) without introducing a temporal-logic DSL.

## Rollout Plan

One PR on `nerdsane/temper` from `feat/action-triggers-unified`. All phases commit atomically inside the branch so reviewers walk the history linearly. No backwards compatibility.

1. **Commit 1** — this ADR.
2. **Commit 2** — Spec layer: `ActionTrigger`, `TriggerKind`, `TriggerLiveness`, `TriggerGuard`, `TargetResolver` structs in `temper-spec`. TOML parser extraction for `[[action.triggers]]`. Delete `Integration`, `AgentTrigger` types and their parse paths.
3. **Commit 3** — Authorization layer: `AuthzEngine::can_principal_potentially_authorize`. Delete `is_system` bypass. Add `system-platform` AgentType + Cedar policy package. Audit and migrate every `AgentContext::system()` caller.
4. **Commit 4** — Verifier composition: `CompositeTemperModel` module, cross-invariant integration, liveness hook. Remove permissive `CrossEntityState → Always` conversion.
5. **Commit 5** — Runtime dispatcher: rename `reaction/` → `trigger/`, unify WASM + webhook execution, principal resolution helper, registry wiring.
6. **Commit 6** — Install + recovery wiring (the ADR-0045 bug fix, folded in): `AppBundle` simplification, `try_register_tenant_with_constraints` signature change, recovery paths.
7. **Commit 7** — Migrations: `temper-fs`, platform agent specs (`plan.ioa.toml`, `task.ioa.toml`), every `[[integration]]` block in the repo. Register new AgentTypes (`file-service`, `agent-supervisor`, etc.) + Cedar policies.
8. **Commit 8** — Tests: parser, authz, composite verifier, runtime dispatcher, e2e DST.

All eight land in one PR.

## Readiness Gates

- `cargo test --workspace` green, including `temper-platform --test platform_e2e_dst`.
- `temper-fs` regression: the three migrated triggers dispatch with byte-identical behavior to the pre-migration `reactions.toml` runtime (same target entities, same params, same resolver IDs).
- Parse-time authority check: spec with a trigger declaring a principal that lacks Cedar authority fails L0 verification.
- Composite L1 runs for any entity whose trigger graph includes at least one cross-entity edge; single-entity fast path runs for entities with no triggers.
- `is_system` bypass regression guard: a test confirms that a `System`-kind principal without an explicit Cedar permit is denied.
- DST replay: two seeded runs under `SimActorSystem` produce identical trigger firing order and identical `Create`-resolver IDs.
- DST reviewer + code-reviewer PASS markers present before commit.
- Pre-push 4-gate pipeline green (rustfmt, clippy, readability ratchet, full test suite).

## Consequences

### Positive
- Cross-entity wiring is visible in the entity spec — first-class, not hidden in a second file.
- One verification pass covers the joint state machine; no snapshot/runtime split for hard cross-invariants.
- No silent privilege escalation; every cross-entity call has an explicit principal (either inherited or named).
- `reactions.toml` deletion removes ~2,300 lines of parallel code (registry, dispatcher, sim_dispatcher, guard evaluator, resolver, params merger, test scaffolding).
- `[[integration]]` deletion removes parallel WASM/webhook declaration path.
- Install and recovery paths no longer need a separate reactions data path; triggers travel with the IOA source through Turso persistence. The four-site install/recovery bug is fixed incidentally by removing the code that can fail.
- Observability improves: each trigger has a name, stable rule-table entry, and tracing span; no WASM-module-as-plumbing opacity.

### Negative
- Breaking change for `temper-fs`, `[[agent_trigger]]` consumers, and every `[[integration]]` block in the repo. Mitigated by migration commits in the same PR.
- `CompositeTemperModel` adds meaningful complexity to `temper-verify`. Scoped to a new module (`composite/`) to contain blast radius.
- Parse-time authority check is conservative — false positives possible when runtime attributes would have allowed the action. Accepted trade-off: false positives surface as spec verification failures the developer can resolve by adjusting the principal or the policy.
- Every `AgentContext::system()` caller needs audit; some may require new Cedar policies under `system-platform`.

### Risks
- **`is_system` bypass removal breaks production callers** that silently relied on it. Mitigation: Phase B of the rollout enumerates every caller via grep and migrates each one with test coverage before deleting the bypass.
- **Joint verification latency for deep trigger chains**. Bounded by `MAX_TRIGGER_DEPTH = 8` and by composition scope being the reachability set (not all entities). Mitigated by tracking state-count in the cascade result and failing fast if the product exceeds a budget.
- **Liveness under fire-and-forget**: `Required` liveness assumes the dispatcher is weakly fair. If the dispatcher is down for long periods, liveness properties are vacuously violated. Mitigation: documented as a dispatcher-availability assumption; apps that cannot tolerate this use a retry-with-deadline pattern outside the verifier.

### DST Compliance
Changes touch simulation-visible crates (`temper-runtime`, `temper-jit`, `temper-server`). Determinism is preserved:

- `CompositeTemperModel` uses `BTreeMap` / `BTreeSet` for all collections.
- `TargetResolver::Create` uses `temper-runtime::sim_uuid()`, not `uuid::Uuid::new_v4`.
- Guard evaluation is a pure function of `(guard, source_fields)` for sync variants; for `cross_entity_state_in`, the sim path reads from the in-memory entity store synchronously.
- No new `static mut`, `lazy_static!`, or `thread_local!`.
- No new `chrono::Utc::now()` or `std::thread::sleep()`.
- Principal elevation produces deterministic `SecurityContext` (all fields populated from one string input).
- `TriggerDispatcher::dispatch_triggers` iterates triggers in declaration order (stable).
- The composite verifier's BFS exploration is deterministic by Stateright's guarantees.

## Non-Goals

- **No expression DSL for guards.** Structured TOML only. If guard expressiveness grows, a future ADR proposes the DSL.
- **No composition across tenants.** Single-tenant verification only.
- **No full temporal-logic surface for liveness.** Windows (`within_ms`), fairness annotations, and complex temporal operators are deferred.
- **No change to `MAX_TRIGGER_DEPTH = 8` or `MAX_GUARD_DEPTH = 4`.**
- **No backwards compatibility.** `reactions.toml`, `[[integration]]`, `[[agent_trigger]]`, `AgentContext::system()` bypass all retire in one PR.
- **No change to WASM module ABI or webhook semantics.** Only their declaration location and dispatch path change.

## Alternatives Considered

1. **Keep reactions in a separate file but fix the install/recovery bug** — Rejected. Fixes the production outage but leaves discoverability broken and the `is_system` bypass in place. The dominant failure mode (developers reaching for WASM instead of reactions) persists.

2. **Fuse reactions into actions but keep `[[integration]]` separate** — Rejected. The user directive in planning was to unify WASM/webhook with entity-dispatch triggers under one primitive. Two overlapping systems ("outgoing effects of an action") is the same discoverability problem at smaller scale.

3. **Make `principal` required on every trigger** — Rejected. Most triggers don't need elevation; requiring a principal for every trigger adds ceremony without safety benefit. Optional-with-inheritance gives the simpler case first-class status and elevation as the exception.

4. **Keep the `is_system` bypass as a platform convenience** — Rejected. Blanket authority grants are incompatible with Cedar's purpose. The `system-platform` AgentType with narrow explicit permits is the right model.

5. **Verify joint dynamics via cross-invariants only, not reaction composition** — Rejected. Cross-invariants are state predicates; they don't cover "does action X eventually enable action Y?" or intermediate states during a cascade. Joint model checking with composed IOAs is strictly more expressive.

6. **Unbounded composition scope** — Rejected. Composing all entities in a tenant into one joint state space is tractable for small apps but blows up for larger ones. Reachability-scoped composition (only entities linked by the trigger graph from the entity being verified) keeps per-spec verification tractable.

## Rollback Policy

If the design proves wrong post-merge, revert the PR. There is no forward-compatible partial rollback because the changes are structural — `[[integration]]`, `[[agent_trigger]]`, and `reactions.toml` are deleted atomically. The PR is designed for atomic revert:

- Reverting restores ADR-0045's world and reinstates the `is_system` bypass.
- `temper-fs` falls back to `reactions.toml`; migrated `[[integration]]` blocks restore.
- `temper-verify` returns to single-entity verification.

Mitigated by full test coverage in the same PR (see Readiness Gates).

If a specific sub-decision proves wrong without requiring full rollback:
- **Liveness hook** — remove `liveness` field from `ActionTrigger`; triggers default to best-effort behavior; existing triggers unaffected.
- **Parse-time authority check** — disable the L0 gate; runtime Cedar still applies.
- **Unified kind discriminator** — not practically rollback-able without reintroducing `[[integration]]`; do not partial-rollback.
