# ADR-0163: GovernanceDecision Callback Mechanism

## Status

Accepted

## Context

When an agent action is denied by Cedar authorization, the platform creates a
`PendingDecision` (legacy, deprecated) and a `GovernanceDecision` entity in the
`temper-system` tenant. A human can then approve or deny the decision via the
Observe UI or channel buttons (Discord, Slack).

The approval flow had accumulated several layers of custom plumbing:

1. **Dead hooks**: `dispatch_custom_effect()` in `hooks.rs` handled
   `GenerateCedarPolicy` and `DeploySpecs` effects, but was never called from
   the dispatch pipeline. `effects.rs` routes custom effects through
   WASM/adapter integration lookup only, and platform entities in
   `temper-system` have no `[[integration]]` sections.

2. **Transport-level orchestration**: Discord (`transport.rs:1219-1278`) and
   Slack (`transport.rs:530-566`) each contained ~40-60 lines of business
   logic querying Sessions by `pending_decision_id` and dispatching
   `ResumeAfterApproval` or `Fail`. This logic was duplicated across both
   transports and violated the Temper-Native rule (transports should be thin
   protocol bridges, not orchestration engines).

3. **Inline policy generation**: `handle_approve_decision` in `decisions.rs`
   generated and loaded Cedar policies inline, duplicating what the
   `GenerateCedarPolicy` hook was supposed to do.

4. **GovernanceDecision entity bypassed**: The entity existed with proper
   states (Pending → Approved/Denied/Expired) and a `GenerateCedarPolicy`
   effect on Approve, but nobody dispatched entity actions on it from the
   button/approval flow.

5. **PendingDecision still primary**: Despite being marked deprecated, the
   `PendingDecision` struct was still the primary approval path with no link
   to the corresponding `GovernanceDecision` entity.

## Decision

### Sub-Decision 1: CustomEffectHandler Trait

Add a `CustomEffectHandler` trait to `temper-server` that `temper-platform`
implements. This respects the crate dependency direction (platform depends on
server, not vice versa) and provides an extension point for platform-level
custom effects.

```rust
// temper-server/src/state/custom_effects.rs
pub trait CustomEffectHandler: Send + Sync {
    fn handle(
        &self,
        effect_name: &str,
        entity_type: &str,
        entity_id: &str,
        entity_fields: &serde_json::Value,
        server: &ServerState,
    ) -> Result<(), String>;
}
```

The dispatch pipeline in `effects.rs` calls this handler after the
WASM/adapter integration block. `PlatformEffectHandler` captures only
`spec_store: Arc<RwLock<SpecStore>>` (not `PlatformState`) to avoid circular
references, and receives `&ServerState` as a parameter when invoked.

### Sub-Decision 2: GovernanceDecision Callback Fields

Extend the GovernanceDecision IOA spec with callback registration:

- **New state variables**: `callback_tenant`, `callback_entity_set`,
  `callback_entity_id`, `callback_on_approve`, `callback_on_deny`,
  `pending_decision_id`
- **New action**: `RegisterCallback` (self-loop on Pending/Approved/Denied) —
  allows any caller to register a callback target before the decision is
  resolved, and safely replay callback delivery if registration happens after
  approval or denial
- **New effect**: `DispatchCallback` on both `Approve` and `Deny` actions —
  when the decision is resolved, the handler reads the callback fields and
  dispatches the registered action on the target entity

The callback mechanism is generic: any entity type can register as a
callback target, not just OpenPaw Sessions.

### Sub-Decision 3: PendingDecision → GovernanceDecision Link

Add `governance_decision_id: Option<String>` to `PendingDecision`. Set it
immediately after GD entity creation in `record_authz_denial()`. The
approve/deny API endpoints use this link to dispatch the corresponding
`GovernanceDecision.Approve` or `GovernanceDecision.Deny` entity action,
which triggers the effect pipeline (GenerateCedarPolicy + DispatchCallback).

### Sub-Decision 4: Thin Transports

Remove Session lookup + ResumeAfterApproval/Fail dispatch from both Discord
and Slack transports. Transports now only:
1. Call the platform's `/api/tenants/{tenant}/decisions/{id}/approve` or
   `/deny` endpoint
2. Update the channel message to show the result

Session resumption is handled entirely by the GovernanceDecision callback
mechanism through the entity effect pipeline.

### Sub-Decision 5: WASM Callback Registration

The `request_approval` WASM module (triggered by `Session.PauseForApproval`)
registers the callback before posting approval buttons:
1. Queries GovernanceDecisions in `temper-system` by `pending_decision_id`
2. Dispatches `RegisterCallback` with the Session entity as target

## Consequences

### Positive

- **Single code path**: Approval resolution flows through one entity-driven
  lane (GovernanceDecision effects) instead of being scattered across
  transports, API handlers, and dead hooks.
- **Transport simplification**: ~100 lines of duplicated orchestration logic
  removed from Discord and Slack transports. Adding a new transport (e.g.,
  Teams) requires zero approval-flow logic.
- **hooks.rs is alive**: The `GenerateCedarPolicy` and `DispatchCallback`
  effects now actually fire through the dispatch pipeline.
- **Generic callback mechanism**: Other entities can register callbacks on
  GovernanceDecision, not just OpenPaw Sessions.
- **Graceful degradation**: If no callback is registered, approval still
  works (policy generated) — the Session just doesn't auto-resume.

### Negative

- **Cross-tenant WASM call**: `request_approval` makes a cross-tenant call
  to `temper-system` to register the callback. This is a proven pattern
  (already used by `record_authz_denial`) but adds one extra HTTP call per
  approval notification.
- **Transitional redundancy**: `handle_approve_decision` still generates
  Cedar policies inline AND dispatches `GovernanceDecision.Approve` which
  triggers `GenerateCedarPolicy` via the effect handler. The inline
  generation handles prospective validation and D-Record creation that the
  hook doesn't do yet. This redundancy can be removed in a future cleanup.

### DST Compliance

`DispatchCallback` uses `tokio::spawn` for the cross-tenant dispatch
(annotated `// determinism-ok: async callback dispatch for governance
decision resolution`). This follows the same pattern as existing scheduled
action dispatch in the codebase.

## Files Modified

### temper-server
- `state/custom_effects.rs` — NEW: CustomEffectHandler trait
- `state/mod.rs` — Register handler field on ServerState
- `state/dispatch/effects.rs` — Call handler after integrations
- `state/pending_decisions.rs` — Add governance_decision_id field
- `authz/helpers.rs` — Link PD to GD after creation
- `api/decisions.rs` — Dispatch GD.Approve/Deny from endpoints

### temper-platform
- `specs/GovernanceDecision.ioa.toml` — Callback fields, RegisterCallback, DispatchCallback
- `specs/model.csdl.xml` — New properties and bound action
- `hooks.rs` — PlatformEffectHandler, DispatchCallback, GenerateCedarPolicy from fields
- `state.rs` — Register handler during construction

### openpaw-codex
- `os-apps/paw-agent/wasm/request_approval/src/lib.rs` — Register GD callback
- `crates/paw-transport/src/discord/transport.rs` — Remove orchestration
- `crates/paw-transport/src/slack/transport.rs` — Remove orchestration
