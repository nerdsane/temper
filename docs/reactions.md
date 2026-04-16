# Cross-Entity Reactions

Reactions are Temper's declarative layer for cross-entity choreography. When a source entity completes an action, a reaction rule dispatches a target action on another entity — no WASM module required.

This document is the developer reference. For architectural rationale see [ADR-0045](adrs/0045-reactions-first-class-app-primitive.md).

---

## When to use a reaction vs. a WASM integration

| | Reactions | WASM integrations |
|---|---|---|
| What triggers it | Another entity's action committing | Another entity's action committing |
| Where it runs | Temper dispatcher (Rust, in-process) | Wasmtime sandbox |
| Authorization | `AgentContext::system()` | Configured principal |
| Failure mode | Fire-and-forget (non-transactional) | Configurable retry / timeout |
| Cascade bound | `MAX_REACTION_DEPTH = 8` | None (bounded by wall-clock timeout) |
| Observable as | Tracing span + `ReactionResult` | Tracing span + integration config |
| Determinism | Yes — deterministic under `SimReactionSystem` with a seed | No — WASM modules treated as side-effect only |

**Use a reaction when:**
- The source action and target action are both on Temper entities.
- The work is "take X from source and call action Y on entity Z."
- You want it to appear in verified traces, reviewable as config, testable without hosting.

**Use a WASM integration when:**
- You need external I/O (HTTP, LLM calls, third-party APIs).
- You need to compute a value the source entity doesn't have.
- The work is deeper than 8 cascade levels.
- You need retry / timeout / backoff semantics.

---

## TOML schema

Reactions live in `reactions.toml` at the app's specs directory. Example structure:

```toml
[[reaction]]
name = "order_confirmed_triggers_payment"

[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"

[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
params = { requested_by = "system" }

[reaction.resolve_target]
type = "same_id"
```

### `[reaction.when]` — trigger

| Field | Type | Required | Meaning |
|---|---|---|---|
| `entity_type` | string | yes | Source entity type (e.g., `Order`) |
| `action` | string | no | Action name — omit to match any action |
| `to_state` | string | no | Required source post-state — omit to match any |
| `guard` | table | no | Conditional predicate (see below) |

### `[reaction.then]` — target action

| Field | Type | Required | Meaning |
|---|---|---|---|
| `entity_type` | string | yes | Target entity type |
| `action` | string | yes | Action to dispatch |
| `params` | inline table | no | Static parameters, merged into the target action's param payload |
| `params_from` | inline table | no | Dynamic params: `target_key = "source_field_name"` — at dispatch, read the named source field and bind it to the target param |

`params` and `params_from` **must not share keys** — that is a parse-time error.

If a `params_from` source field is missing on the source entity at dispatch time, the key is logged (`tracing::warn!`) and skipped; the reaction still fires with a partial param map.

### `[reaction.resolve_target]` — how to pick the target entity ID

| `type` | Required fields | Behavior |
|---|---|---|
| `field` | `field` | Read the target entity ID from a source field. Missing → reaction skipped (warn). |
| `same_id` | — | Target ID = source entity ID. |
| `static` | `entity_id` | Target ID is a fixed string. Useful for per-tenant singletons. |
| `create_if_missing` | `id_field` | Read target ID from source field; if absent, derive `"{source_id}-derived"`. Good for per-source-entity singletons (e.g., one `FileVersion` per `File`). |
| `create` | — | Fresh UUID on every dispatch via `sim_uuid()`. Correct choice for pipeline chaining where each source action spawns a brand-new target instance. |

---

## Guards

`[reaction.when.guard]` is an optional predicate that gates firing. Guard-skipped rules do **not** emit a `ReactionResult` — they never fired.

### Source-field guards (sync, cheap)

```toml
[reaction.when.guard]
type = "field_equals"
field = "job_type"
value = "source_search"
```

| `type` | Fields | Behavior |
|---|---|---|
| `field_equals` | `field`, `value` | Source field JSON-equals `value` |
| `field_in` | `field`, `values` (array) | Source field ∈ `values` |
| `bool_true` | `field` | Source field is JSON `true` |
| `bool_false` | `field` | Source field is JSON `false` |
| `state_in` | `values` (array) | Source post-status ∈ `values` — complements `to_state` when multiple states are acceptable |

Missing source fields on any of the above evaluate to `false` with a `tracing::debug!`.

### Cross-entity guard (async, one fetch per guard)

```toml
[reaction.when.guard]
type = "cross_entity_state_in"
entity_type = "Workspace"
entity_id_source = "workspace_id"
required_status = ["Active"]
```

Reads `workspace_id` from the source entity, fetches `Workspace:{workspace_id}` via the existing `resolve_entity_status` helper (same path IOA guards use), compares against `required_status`. Missing target ID or fetch failure evaluates to `false` with a `tracing::warn!` — stricter than IOA's vacuous-truth handling, since an empty target ID is almost always a misconfiguration.

### Composite guards

```toml
[reaction.when.guard]
type = "all_of"
guards = [
  { type = "bool_true", field = "ready" },
  { type = "cross_entity_state_in",
    entity_type = "Workspace",
    entity_id_source = "workspace_id",
    required_status = ["Active"] },
]
```

`all_of` / `any_of` / `not` compose into `MAX_GUARD_DEPTH = 4` (validated at parse time).

---

## Three example patterns

### 1. Pipeline chaining

Each `CurationJob` completion spawns the next stage as a new job:

```toml
[[reaction]]
name = "source_search_complete_triggers_rank"

[reaction.when]
entity_type = "CurationJob"
action = "Complete"
[reaction.when.guard]
type = "field_equals"
field = "job_type"
value = "source_search"

[reaction.then]
entity_type = "CurationJob"
action = "Submit"
params = { job_type = "rank" }
params_from = { input = "output" }

[reaction.resolve_target]
type = "create"
```

Fresh UUID for the new job, `output` from the source piped into the target's `input`.

### 2. Session-completion callback

When a child `Session` completes, Ack the parent — but only if the parent is still Active:

```toml
[[reaction]]
name = "session_complete_acks_parent"

[reaction.when]
entity_type = "Session"
action = "Complete"
[reaction.when.guard]
type = "cross_entity_state_in"
entity_type = "Workspace"
entity_id_source = "workspace_id"
required_status = ["Active"]

[reaction.then]
entity_type = "Workspace"
action = "AckSession"
params_from = { session_id = "id" }

[reaction.resolve_target]
type = "field"
field = "workspace_id"
```

### 3. Cleanup-on-failed

When an entity enters Failed, clean up its related resources:

```toml
[[reaction]]
name = "order_failed_releases_inventory"

[reaction.when]
entity_type = "Order"
action = "FailOrder"
to_state = "Failed"

[reaction.then]
entity_type = "InventoryHold"
action = "Release"
params_from = { order_id = "id" }

[reaction.resolve_target]
type = "field"
field = "hold_id"
```

---

## Invariants (what does NOT change)

These properties are load-bearing and unchanged by any of the four Phase additions:

- **Fire-and-forget.** A failing reaction does NOT roll back the source transition. The source action is already committed by the time the dispatcher runs.
- **Cascade bound.** `MAX_REACTION_DEPTH = 8` caps the depth of recursive reaction chains. Beyond 8, further reactions are dropped with a warning.
- **Tenant isolation.** Reactions only fire for rules registered under the same tenant as the source action.
- **System principal.** Target actions dispatched by reactions run under `AgentContext::system()`, not the source action's principal.
- **Determinism under `SimReactionSystem`.** Two seeded runs with the same inputs produce the same reaction firing order and the same `create`-resolver IDs.
- **Budget.** `MAX_REACTIONS_PER_TENANT = 256`, `MAX_GUARD_DEPTH = 4`.

---

## Where reactions fit in the Temper architecture

Reactions are the *composition* layer. Actions are the *contract* layer.

- **Actions** are the entity's verified surface — preconditions, guards, effects, invariants. Each entity is a closed I/O Automaton, which is why `temper-verify` can model-check each action in isolation.
- **Reactions** are how apps wire entities together. They fire *after* actions commit, elevated to the system principal, non-transactional, bounded.

Keeping the layers separate is what makes verification tractable (each entity stays a closed automaton), authorization coherent (the user who submitted an Order doesn't inherit authority over Payment), and cascades bounded. See [ADR-0045 Sub-Decision 5](adrs/0045-reactions-first-class-app-primitive.md#sub-decision-5-keep-reactions-separate-from-actions-architectural-reaffirmation) for the full rationale.

The only production consumer of hand-authored reactions today is `os-apps/temper-fs/reactions/reactions.toml`. Apps that need cross-entity choreography should prefer reactions over WASM integrations unless they need computation, external I/O, or retries.
