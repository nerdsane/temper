# ADR-0128: Directed Evolution Policy-Gated Repair Autostart

- Status: Accepted
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0120: Directed Evolution Control Plane
  - ADR-0124: Directed Evolution Promotion Materialization
  - ADR-0127: Directed Evolution Materialization-Gated Completion
  - `os-apps/directed-evolution/wasm/work_item_result_router`
  - `os-apps/directed-evolution/specs/autonomy_policy.ioa.toml`

## Context

Directed Evolution currently records the discovery side of the loop: a signal
creates an observer work item, the observer brain returns a pressure and
direction, and Mission Control can show the proposed direction. The pipeline
still requires an external caller to create an episode, record the episode
contract, select the direction, and start the episode.

That is correct for product growth and policy changes, because the human
director must negotiate the Adaptation Goal and Viability Constraints in chat.
It is incomplete for bounded repair pressure. The active autonomy policy for
Agent Answers explicitly allows repair pressure to proceed automatically after
evidence while growth and policy pressure remain human-gated. Without an
autostart bridge, the policy is visible but inert: repair directions stop at
`Direction.Proposed`.

## Decision

When the observer brain returns an actionable repair direction, the
`work_item_result_router` will materialize and start an episode only if both
conditions are true:

1. The brain-produced lane is automatic repair (`repair-auto`, or equivalent
   text containing repair and auto/automatic).
2. The active `AutonomyPolicy` for the organism permits automatic repair and
   does not mark the repair lane as human-gated or blocked.

If either condition is false, the router leaves the direction proposed. Growth,
UX, policy, and data-model directions continue to wait for explicit human
approval in chat.

For policy-approved repairs, the router creates the standard episode scaffold:

- `Episode.BeginEpisodeNegotiation`
- `AdaptationGoal.ActivateAdaptationGoal`
- one or more `ViabilityConstraint.ActivateViabilityConstraint`
- `SelectionPressure.ActivateSelectionPressure`
- `EvaluationStage.ActivateEvaluationStage` for code/spec review and AI
  simulated-user trial
- `Episode.RecordEpisodeContract`
- `Direction.SelectDirection`
- `Episode.StartEpisode`

The contract is derived from observer-brain output where present:
`proposed_adaptation_goal`, `proposed_viability_constraints`,
`selection_statement`, and optional `evaluation_stages`. Bounded repair
fallbacks are used only when the brain omitted a field, and those fallbacks
preserve the organism's baseline behavior and require the simulated-user trial.

**Why this approach**: The brain still identifies the pressure and proposes the
direction. The deterministic WASM router only enforces policy and writes the
state-machine records that make the approved autonomy lane real. This keeps the
human gate intact for growth while allowing emergency/repair pressure to move.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add policy-gated repair autostart to the observer
   result router, rebuild Directed Evolution WASM, and hot-load the app bundle.
2. **Phase 1 (Proof)** — Run a live isolated repair signal proof showing a
   `repair-auto` direction transitions through episode start and queues variant
   generation without human approval.
3. **Phase 2 (UX)** — Mission Control already shows autonomy lanes and episode
   state; verify the auto-started repair episode appears as live state.

## Readiness Gates

- Growth directions remain proposed and do not auto-start.
- Repair directions do not auto-start without an active permitting policy.
- Auto-started repair episodes have an Adaptation Goal, Viability Constraints,
  Selection Pressure, and evaluation stages before `StartEpisode`.
- `StartEpisode` still triggers the existing episode orchestrator and queues
  real variant-generator work items.

## Consequences

### Positive

- The visible Autonomy Policy now moves real state, not only dashboard text.
- Bounded repair pressure can reach generation work without a human click.
- Mission Control can truthfully show whether a direction was human-gated or
  auto-started.

### Negative

- The observer router now performs more orchestration work after a brain result.
- Fallback contracts are necessarily generic until the observer brain supplies
  richer repair-specific criteria.

### Risks

- A misclassified growth direction could auto-start if both the brain and policy
  text incorrectly mark it as automatic repair. Mitigation: require both repair
  and automatic semantics, and reject lanes containing growth, policy, UX,
  feature, product, or data-model terms.
- Missing active policy could silently leave repair directions proposed.
  Mitigation: return routing metadata that names whether autostart was skipped
  and why.

### DST Compliance

This change is limited to Directed Evolution WASM and tests. It does not touch
simulation-visible runtime crates.

## Non-Goals

- No growth/product-feature autostart.
- No UI control for approving growth directions.
- No replacement for human-brain chat negotiation.
- No direct Codex execution inside Temper or Railway.

## Alternatives Considered

1. **Always auto-start repair lanes from observer output** — rejected because
   the user asked that Mission Control clearly reflect what is authorized, and
   autonomy must be policy-backed.
2. **Require a separate planner work item before repair start** — rejected for
   V1 because repair pressure is intended to proceed automatically after
   evidence, and the observer brain already produced the repair direction and
   proposed constraints.
3. **Put autostart in Mission Control** — rejected because the UI should remain
   primarily observational and the policy decision belongs in the control plane.

## Rollback Policy

Remove the observer-router autostart call and rebuild/hot-load the previous
Directed Evolution WASM. Existing directions and episodes remain valid; future
repair directions will stop at `Proposed` again.
