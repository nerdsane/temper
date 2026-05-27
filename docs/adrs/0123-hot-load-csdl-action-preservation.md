# ADR-0123: Hot-Load CSDL Action Preservation

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0122: Genesis pinned app install
  - `crates/temper-spec/src/csdl/parser/schema.rs`
  - `crates/temper-spec/src/csdl/merge.rs`
  - `crates/temper-server/src/registry/mod.rs`

## Context

Genesis app installs hot-load Temper-native apps into an already running tenant. First-time installs can serve the app's raw `model.csdl.xml`, but updates into an existing tenant parse incoming CSDL, merge it with the tenant registry, and re-emit a normalized metadata document.

Directed Evolution exposed a metadata gap during promotion of the Agent Answers organism. The winning app version added `Answer.Calibrate`. The hot-loaded runtime accepted and executed the action because the IOA transition table was swapped correctly, but `$metadata` in the existing `default` tenant omitted the newly added bound action. The cause was that some generated app CSDL nests `<Action>` elements inside `<EntityType>`. This shape is tolerated by first-install raw metadata but was ignored by the parser during merge/re-emit, causing actions to disappear from discoverability even though the runtime could execute them.

Hot-load must preserve both executable behavior and discoverable behavior. Mission Control, simulated users, and future brain agents should be able to rely on `$metadata` after promotion instead of needing first-install raw CSDL or trial-and-error action dispatch.

## Decision

Temper will tolerate entity-nested bound actions in parsed CSDL by promoting them into the schema-level action list during parsing. The normalized emitter already emits actions at schema scope, which is the canonical OData shape used by the rest of the parser and merge code.

This keeps runtime install semantics unchanged:

- IOA remains the executable source of transition behavior.
- CSDL remains the discoverability surface for entity sets, properties, and bound actions.
- Merge mode can continue using parsed-and-emitted CSDL without losing actions from app versions whose source CSDL used the older nested style.

## Rollout Plan

1. **Phase 0 (Immediate)** — Update the CSDL parser to collect `<Action>` children under `<EntityType>` and append them to the owning schema's `actions`.
2. **Phase 1 (Verification)** — Add parser and merge tests proving nested actions survive parse, merge, and emit.
3. **Phase 2 (Production)** — Deploy the platform fix once, then re-hot-load the promoted app version into the existing production tenant without replacing tenant state.

## Readiness Gates

- `$metadata` for an existing tenant shows newly added bound actions after an app hot-load.
- The same action is executable against existing tenant entities after state-preserving promotion.
- Focused `temper-spec` tests pass.

## Consequences

### Positive

- Existing generated apps with nested action CSDL remain compatible.
- Hot-load into existing tenants preserves metadata discoverability.
- Directed Evolution promotions do not require tenant replacement just to refresh metadata.

### Negative

- The parser accepts a non-canonical CSDL shape for backward compatibility.

### Risks

- If a schema contains both a schema-level action and an entity-nested action with the same name, normal merge/replace-by-name semantics will choose the later parsed value. This mirrors existing incoming-wins behavior.

### DST Compliance

- The change is in `temper-spec` parsing and does not touch simulation-visible runtime execution. It introduces no wall-clock, randomness, filesystem, network, or concurrency behavior.

## Non-Goals

- This ADR does not make nested actions the preferred authoring style.
- This ADR does not change IOA transition semantics.
- This ADR does not redesign CSDL generation for Genesis apps.

## Alternatives Considered

1. **Require every app to rewrite CSDL with schema-level actions** — Rejected for the immediate fix because already-published app versions and generated variants can still use nested actions.
2. **Serve raw incoming CSDL after merge** — Rejected because existing tenants may contain multiple apps and schemas; the registry needs a normalized merged CSDL document.
3. **Ignore `$metadata` and rely on `@odata.actions`** — Rejected because metadata is the contract agents and UIs use before they have a concrete entity instance in a particular state.

## Rollback Policy

Revert the parser tolerance and re-hot-load only app versions whose CSDL already uses schema-level actions.
