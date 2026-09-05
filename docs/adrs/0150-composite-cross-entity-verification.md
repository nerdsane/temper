# ADR-0150: Always-On Composite Cross-Entity Verification

- Status: Accepted
- Date: 2026-06-22
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Unified action triggers + composite trigger-graph (introduced `CompositeVerificationPlan` / `CompositeTemperModel`).
  - ADR-0149: Free-boolean cross-entity guards (per-entity L1 offers cross-entity-gated edges).
  - `crates/temper-verify/src/composite/` (joint-state BFS verifier).
  - `crates/temper-cli/src/verify/` (the `temper verify` command).
  - `crates/temper-spec/src/automaton/` (trigger parsing + trigger graph).

## Context

Cross-entity reactions are the backbone of Temper apps: when one entity's action
commits, an inline `[[action.triggers]]` block of `kind = "entity"` dispatches an
action on a *different* entity (fire-and-forget, post-commit). ADR-0046 added a
trigger graph and a joint-state model (`CompositeTemperModel`) that can BFS the
product state space and apply these reaction cascades inside each step.

But that model was **opt-in and report-only**:

- `VerificationCascade` only built a composite *report* when explicitly configured
  with `with_composite_scope(...)`, and that report never failed the cascade.
- The `temper verify` CLI never enabled it. Directory verification ran a per-entity
  cascade on each spec independently — it never composed entities.

The consequence: a whole class of bug is invisible. A reaction can arrive at its
target when the target has already left the state the reaction requires (a
*from-state mismatch*), or when the target's guard is false. The runtime treats
this as fire-and-forget and silently drops the dispatch. Nothing — not the
per-entity checker, not the CLI — surfaces it. The intended state change simply
never happens, and the spec looks "verified".

Two concrete instances live in `os-apps/temper-fs`:

- `File.StreamUpdated` fires `Workspace.IncrementUsage`. `IncrementUsage` is only
  enabled from `Active`. If the workspace is `Frozen`, the usage increment is
  dropped — `used_bytes` silently drifts from reality.
- `File.StreamUpdated` fires `FileVersion.Supersede` on `last_version_id`.
  `Supersede` is only enabled from `Current`. A version that is already
  `Superseded` (double-supersede) drops the reaction.

## Decision

Make composite cross-entity verification **always run** during directory/bundle
verification, as a first-class **gating** step, and add a property that catches
dropped reactions.

### Sub-Decision 1: Composite is a gating cascade step for multi-entity specs

When `temper verify` is given a spec **directory** (or bundle) that parses to two
or more entities, it builds composite plans and runs joint-state BFS as part of
the command. A composite failure **fails the command** (non-zero exit) — it is
not report-only.

Single-spec / stdin verification (`temper verify-ioa`, the subprocess endpoint)
stays per-entity. Composite verification is inherently a multi-entity, dir/bundle
concern: there is nothing to compose from one spec.

**Why directory-level only**: the joint model needs every participating entity's
automaton in hand. Stdin delivers exactly one. Composing requires the full set the
directory provides.

### Sub-Decision 2: Seed cover — verify every entity

The composite verifier is seeded from the **root of each weakly-connected
component** of the entity trigger graph. Every entity belongs to exactly one
weakly-connected component, so this seed set covers the whole graph: no entity is
left unverified, and entities already covered by a larger component's plan are not
re-seeded redundantly.

Concretely: compute weakly-connected components over the undirected projection of
the trigger graph; for each component pick a deterministic root (the
lexicographically smallest entity name, for DST-stable output); build one
`CompositeVerificationPlan` per root. Isolated entities (no edges) form
singleton components and are still seeded — they verify as a trivial one-entity
composite, identical to their per-entity run.

**Why weakly-connected, not strongly-connected**: reactions flow in one direction
along an edge, but a from-state mismatch can occur regardless of which end you
seed from. A weakly-connected component is the set of entities that can influence
each other's reachable joint state through *any* chain of reactions; seeding its
root reaches all of them via `reachable_from`. Using the directed reachability
closure from a single arbitrary seed would miss entities that only *send* to the
seed's component without being reachable *from* the seed.

### Sub-Decision 3: The `no_dropped_reaction` property

During the joint BFS, every time an entity action fires an entity-trigger
(a reaction) whose target action is **not enabled from the target entity's current
state** — because of a from-state mismatch or a false guard — that is a **dropped
reaction**. The composite model records it as a property violation with a
counterexample naming:

- source entity + source action,
- target entity + target action,
- the target entity's current state at the moment of the drop,
- the trigger name.

This runs inside `CompositeTemperModel::next_state` (where cascades already fire),
so it sees exactly the joint states the BFS reaches.

**Exemptions:**

- **`create` resolvers are exempt.** A `resolve_target = { type = "create" }`
  trigger spawns a *fresh* target instance, which is always in its initial state
  and therefore always enabled for its `Create` action. There is no existing
  target to be "in the wrong state", so a create-resolver reaction can never be
  dropped. (`create_if_missing` is treated like a normal field resolver: it may
  hit an existing, wrongly-stated target.)

- **`drop_ok = true` suppresses the violation.** Some best-effort reactions are
  *intended* to be dropped when the target is not ready (e.g. a notification that
  is meaningless once an entity is archived). An author marks the trigger
  `drop_ok = true`; the composite verifier then treats a drop on that trigger as
  expected and emits no violation. This is opt-in and per-trigger — the default
  (`drop_ok = false`) is "a dropped reaction is a bug".

### Sub-Decision 4: `drop_ok` parser addition

Add a single optional boolean field `drop_ok` to `ActionTrigger`
(`#[serde(default)]`, defaults `false`). Triggers are deserialized via serde, so
no hand-rolled parser change is needed; the field is additive and backward
compatible (existing specs omit it and get `false`). It is threaded into
`TriggerEdge` (so the composite model, which reads edges, can see it) by
`edge_from_trigger`.

### Sub-Decision 5: Bounded joint-state BFS

The product state space can be large. The composite BFS is bounded with a target
state-count budget (mirroring the per-entity checker, which already inspects
`is_done()` after a bounded `spawn_bfs`). If the checker stops before exhausting
the space (`!is_done()`), the result is marked **INCOMPLETE**: the command emits
a warning, does not claim a pass, and surfaces that the proof is partial. An
incomplete run never silently passes — it is honestly reported as not fully
explored. Discovered violations from a partial run are still real and still gate.

### Sub-Decision 6: `required` cross-entity ref — empty ref fails, not vacuous (ARN-92 #2)

The runtime cross-entity guard resolver (`state/dispatch/cross_entity.rs`) treats
an empty/missing scalar ref or an empty list relation as a **vacuous pass** —
there is nothing to check, so the guard holds. That is correct for an *optional*
relationship, but wrong for a *required* one: a `cross_entity_state` guard whose
`entity_id_source` was never set should fail (the precondition cannot be
satisfied by an absent target), not silently pass.

Add an optional `required` attribute to the `cross_entity_state` guard
(`#[serde(default)]`, defaults `false`, threaded parser → `ResolvedGuard` → JIT
`Guard::CrossEntityStateIn` → `collect_cross_guards`). When `required = true`,
the resolver inserts `(key, false)` for an empty scalar or empty list ref instead
of `(key, true)`. Optional refs (the default) keep the vacuous-true behavior, so
the existing blast radius — e.g. an optional list relation with no members — is
unchanged. The L1 model is unaffected: the cross-entity status is already a free
boolean there (ADR-0149), so the empty-ref distinction is purely a runtime
resolution concern.

## Consequences

- Multi-entity apps now get their cross-entity reaction ordering checked on every
  `temper verify`. The two temper-fs drops above (and the structural double-
  supersede) surface as gating failures.
- A required cross-entity ref that was never set now fails its guard at runtime
  instead of passing vacuously (ARN-92 #2), closing a hole where a missing
  relationship silently satisfied a precondition.
- Authors gain a precise vocabulary for intentional best-effort drops (`drop_ok`)
  and a verifier that holds them to it everywhere else.
- The budget keeps verification bounded and honest: large products report
  INCOMPLETE rather than passing vacuously or hanging.

## Alternatives Considered

- **Keep composite report-only, add a lint.** Rejected: a lint over static trigger
  shapes cannot see *reachable joint states* — whether the target is actually in
  the wrong state when the reaction arrives is a state-space property, not a
  syntactic one.
- **Seed only from sources with no incoming edges.** Rejected: cyclic components
  (reaction feedback loops, legal under `MAX_TRIGGER_DEPTH`) have no such source;
  weakly-connected-component roots always exist and always cover.
