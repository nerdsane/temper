# ADR-0018: Spec-Free Actions, Platform Presets, and Trace Visualization

- Status: Accepted
- Date: 2026-02-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0013: Evolution Loop Agent Integration
  - ADR-0014: Governance Gap Closure
  - ADR-0015: Agent OS Cross-Entity Primitives
  - `crates/temper-server/src/state/dispatch.rs`
  - `crates/temper-server/src/odata/bindings.rs`
  - `crates/temper-authz/src/engine.rs`
  - `crates/temper-spec/src/automaton/`
  - `crates/temper-verify/src/cascade.rs`

## Context

Temper requires a state machine spec for every entity type. When an agent dispatches an action on an entity type with no registered spec, the system returns "No transition table" and the action fails. This is a straitjacket: agents cannot do unstructured work (research, debugging, brainstorming) without first generating and submitting a formal spec.

Additionally, the verification cascade proves specs are *self-consistent* but not *organizationally correct*. There is no mechanism to enforce structural rules on specs (e.g., "deploy transitions require guards") or to let developers visualize reachable state paths.

Three gaps are addressed:

1. **Straitjacket problem**: Agents need to act on entity types that don't have specs.
2. **Spec quality enforcement**: Organizations need to enforce structural invariants on specs.
3. **Trust gap**: Developers need to see reachable paths before approving spec changes.

## Decision

### Sub-Decision 1: Spec-Free Actions (Feature 1C)

Two dispatch modes, determined by whether a spec exists for the entity type:

| Mode | When | Cedar | State Machine | Trajectory |
|------|------|-------|---------------|------------|
| Spec-free | No spec registered | Enforced (default-deny) | None | Record action + outcome |
| Spec-governed | Spec registered | Enforced (default-deny) | Full enforcement | Record action + outcome |

**Cedar governs both modes.** The difference is purely whether a state machine exists.

When no TransitionTable exists and Cedar allows the action:
1. Record trajectory entry with `spec_governed: false`
2. Return 200 with `{ "action": "...", "recorded": true, "specGoverned": false }`
3. No entity created, no state tracked — trajectory only

**Why this approach**: Governance (Cedar) is orthogonal to specification (state machine). An action can be Cedar-authorized without needing a spec. This preserves the security model while removing the requirement that all work be state-machine-shaped.

### Sub-Decision 2: Platform Presets (Feature 2A)

Platform-level Cedar `forbid` policies enforce rules at two evaluation points:

1. **Dispatch presets**: Which action types REQUIRE a spec (evaluated at action time).
2. **Spec quality presets**: Structural rules on specs (evaluated at spec submission time).

Both use Cedar `forbid` — tenants cannot weaken them.

Dispatch presets check `context.has_spec` at dispatch time. Spec quality presets check `context.state_count`, `context.invariant_count`, `context.guarded_target_states`, etc. at spec submission time.

To support spec quality rules, a `SpecMetadata` struct is extracted from parsed `Automaton` types and flattened into Cedar context attributes. Cedar set support is added to handle array-valued attributes (e.g., `states.contains("Deployed")`).

**Why this approach**: Reuses the existing Cedar infrastructure. `forbid` overrides `permit` by design, making platform presets non-negotiable. Spec metadata extraction decouples spec analysis from policy evaluation.

### Sub-Decision 3: Trace Visualization (Feature 2B)

Post-hoc BFS on `TemperModel` (after L1 model check passes) extracts shortest paths to target states. Uses the `Model` trait API — no Stateright internals.

Results include: paths per target state, unreachable states, terminal states, and BFS stats. Available as an optional enrichment to `CascadeResult` and via a standalone API endpoint.

**Why this approach**: BFS is simple, deterministic, and bounded. Operating post-L1 means the model is already verified. No dependency on Stateright internals — uses only public `TemperModel` types.

## Rollout Plan

1. **Phase 0 (Immediate)**: Foundation types + spec-free dispatch + platform presets + path extraction
2. **Phase 1 (Follow-up)**: UI integration for trace visualization
3. **Phase 2 (Production)**: Default platform presets for common organizational patterns

## Consequences

### Positive
- Agents can act on unspecced entity types (research, debugging, brainstorming)
- Organizations enforce structural invariants via Cedar (no code changes)
- Developers visualize reachable paths before approving spec changes
- All actions remain Cedar-governed — no ungoverned mode

### Negative
- Spec-free actions have no state tracking (trajectory only)
- Platform presets add Cedar policy complexity
- Path extraction adds CPU cost to verification (bounded by config)

### Risks
- Agents might over-rely on spec-free mode, avoiding specs when they should be written. Mitigated by platform presets that require specs for consequential actions.

### DST Compliance
- `SpecMetadata` extraction: pure function, no I/O
- Path extraction: `BTreeMap` for parent tracking (deterministic iteration)
- Dispatch changes: uses `sim_now()` for trajectory timestamps (existing pattern)
- Cedar HashMap at engine boundary: `// determinism-ok` (existing pattern, Cedar API requires HashMap)

## Non-Goals

- Real-time path computation during dispatch (too expensive)
- Automatic spec generation from spec-free action patterns (future work)
- UI for trace visualization (separate PR)

## Alternatives Considered

1. **Tier system for entity types** — Explicit "unstructured" vs "structured" tiers. Rejected: creates a configuration burden. The natural check is "does a spec exist?"
2. **Separate unstructured action API** — New endpoint like `/unstructured/Tasks/complete`. Rejected: fragments the API surface. Better to handle it in the existing dispatch path.
3. **TLA+ trace extraction** — Use TLA+ toolchain for path visualization. Rejected: TLA+ is legacy; the IOA model is primary.
