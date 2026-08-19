# ADR-0171: Verify related-field sidecar constraints

- Status: Accepted
- Date: 2026-08-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Sub-Decision 6 promised hard sidecar rows as joint properties (not shipped)
  - ADR-0150: JCS (`joint_local_invariants`, `no_dropped_reaction`)
  - ADR-0170: Constraint mechanism map (names)
  - [ARN-387](https://linear.app/arni-build/issue/ARN-387): this work
  - `crates/temper-spec/src/cross_invariant/` (sidecar parse/lint)
  - `crates/temper-verify/src/composite/` (joint checker)
  - `crates/temper-cli/src/verify/` (`temper verify --specs-dir`)

## Context

`cross-invariants.toml` is the related-field / relation-policy sidecar. The write path already parses it (`constraints.rs`, `EventualInvariantTracker`). `temper verify` did not.

ADR-0046 Sub-Decision 6 said hard-kind rows become joint properties on `CompositeTemperModel`. That translation never landed. JCS (ADR-0150) therefore proved local `[[invariant]]`s after reactions and that entity-kind triggers land enabled — and said nothing about a sidecar that can exist next to the specs.

Rita’s fixture (`docs/study/publish-needs-review/`): HumanCurator.Publish is enabled by a local flag. The sidecar requires `related(ReviewAgent, review_agent_id).status in ["VerdictRecorded"]`. The two machines have no reactions, so trigger-graph composition verified them in isolation and the related() target was out of scope. The write path would reject Publish; verify would pass.

If the sidecar is on disk, verify must verify it.

## Decision

### Sub-Decision 1: Hard rows are an enabled-action property

A hard sidecar rule fails verify when a matching action is **enabled** in a reachable joint state while `related(...).field` does not hold.

Do **not** synthesize sidecar rows as extra guards on `actions()`. That hides the bug: the broken Publish spec would pass because Publish would simply not be offered.

`on` is `Entity.*` or `Entity.Action` (`split_trigger`). Matching uses the same enabled set the joint model already advertises, including resolved `cross_entity_state` guards.

**Why this approach**: The property names the spec author wrote. A missing `cross_entity_state` on Publish is a verify failure, not a silent shrink of the action set.

### Sub-Decision 2: Composition unions related() pairs

`seed_cover` and plan reachability stay trigger-graph based, and also union undirected edges `on` entity ↔ `related()` target (hard rows only). Those entities share one seed and one joint state.

Resolution is one instance per entity type (ADR-0046): `related(ReviewAgent, review_agent_id)` reads the ReviewAgent slice’s named field (v1: `status`; also a bool or counter of that name when present on `TemperModelState`). The FK is not solved.

If the target type is not in the supplied automaton set, do not silently pass: fail plan-build or report `Incomplete` with the reason. Prefer including the target via the new edges when the spec is present.

If a named field cannot be read from `TemperModelState`, fail closed on that rule (named violation). Do not skip it.

**Why this approach**: HumanCurator and ReviewAgent have no reactions. Without the related() edge they never share a seed, so the property cannot see the ReviewAgent slice.

### Sub-Decision 3: Eventual-kind stays runtime

Eventual rows still use `EventualInvariantTracker`. Verify warns once and does not gate on them.

**Why this approach**: Eventual needs a convergence window, not a snapshot of reachable states. Same as ADR-0046 Sub-Decision 6.

### Sub-Decision 4: Directory verify loads the sidecar; `verify-ioa` does not

`temper verify --specs-dir` reads optional `cross-invariants.toml` with the same `parse_cross_invariants` + `lint_cross_invariants` as the serve loader. The parsed spec is passed into `verify_all`. No file means today’s behavior.

`temper verify-ioa` stays single-entity. There is nothing to compose.

The on-disk name `cross-invariants.toml` and the `CrossInvariant*` Rust types stay (ADR-0170). New prose says related-field constraint.

## Consequences

### Positive

- An unguarded Publish that the sidecar forbids fails `temper verify` and names the row (e.g. `PublishNeedsThisReviewRecorded`).
- Adding a `cross_entity_state` guard that matches the sidecar makes the same joint space pass.
- Directory verify and the write path now read the same file.

### Negative

- Isolated entities linked only by the sidecar share a product state space (one instance each). That is larger than today’s per-entity plans, still small for the one-instance model.
- Field equality (`related(...).Field == this.Field`, ARN-299) is still impossible.

### Risks

- Someone later “fixes” a FAIL by filtering `actions()` with the sidecar. That would make the broken fixture pass. The property must stay an enabled-action check.

## Non-Goals

- ARN-299 field-equality cross-entity guards.
- Feeding eventual-kind rows into the checker.
- Renaming `cross-invariants.toml`, `CrossInvariant*`, or `TEMPER_XINV_*`.
- L2b, CSDL generation, temper-evolution.

## Alternatives Considered

1. **Synthesize sidecar rows as extra `actions()` guards** — Rejected. The broken Publish spec would pass because Publish would not be enabled.
2. **Keep trigger-only composition and skip the rule when the target is out of scope** — Rejected. That is today’s silent pass.
3. **Require authors to add a dummy reaction so the pair composes** — Rejected. The sidecar already names the pair.

## Rollback Policy

Revert this ADR and the composite/CLI wiring. The write path is unchanged.
