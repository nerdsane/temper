# Cross-Entity Reactions

Reactions are Temper's declarative layer for cross-entity choreography. When a source entity completes an action, a reaction dispatches a target action on another entity — no WASM module required.

A reaction is an `[[action.triggers]]` entry with `kind = "entity"`, declared on the source action. `[[action.triggers]]` is the only way an action causes work elsewhere: the other kinds are `wasm`, `adapter`, `webhook` and `hook` (a platform hook registered by the host, e.g. `hook = "DispatchCallback"`). Standalone `reactions.toml` files, `[[integration]]` blocks and `trigger`/`emit` effects are retired; an app that still ships a `reactions.toml` fails to install.

This document is the developer reference. For architectural rationale see [ADR-0045](adrs/0045-reactions-first-class-app-primitive.md), [ADR-0046](adrs/0046-unified-action-triggers.md) and [ADR-0180](adrs/0180-unified-effect-syntax.md).

---

## When to use a reaction vs. a WASM integration

| | Reactions | WASM integrations |
|---|---|---|
| What triggers it | Another entity's action committing | Another entity's action committing |
| Where it runs | Temper dispatcher (Rust, in-process) | Wasmtime sandbox |
| Authorization | Invoking principal, or the trigger's `principal` | Invoking principal, or the trigger's `principal` |
| Failure mode | Fire-and-forget (non-transactional) | Configurable retry / timeout |
| Cascade bound | A chain stops at its first repeated step (same action on the same entity) | None (bounded by wall-clock timeout) |
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

A reaction is declared on the source action in its `.ioa.toml`:

```toml
[[action]]
name = "ConfirmOrder"
from = ["Submitted"]
to = "Confirmed"

[[action.triggers]]
name = "order_confirmed_triggers_payment"
kind = "entity"
guard = "status == 'Confirmed'"
target_entity = "Payment"
target_action = "AuthorizePayment"
args = { requested_by = "'system'", order_id = "Id" }
resolve_target = { kind = "same_id" }
```

### When it fires

| Field | Type | Required | Meaning |
|---|---|---|---|
| (enclosing `[[action]]`) | — | — | The source entity type and action |
| `guard` | string | no | Predicate over the source's post-action `status` and fields (see below); omit to fire on every commit |

### Target action

| Field | Type | Required | Meaning |
|---|---|---|---|
| `target_entity` | string | yes | Target entity type |
| `target_action` | string | yes | Action to dispatch |
| `principal` | string | no | Registered `AgentType` to dispatch as; omit to inherit the invoking principal |
| `args` | table | no | Parameters for the target action: `target_param = "value"`, each value in the [predicate grammar](predicates.md) |

Each `args` value is a literal — `'text'` in single quotes, an integer, `true`, `false` or `null` — or the name of a source field, read after the source commits (`Id` is the source entity's id). A declared variable the source record stores no value for, because the record is older than the declaration, reads as its declared initial value, as it does in a guard. A name must be a declared state variable, `Id`, or a parameter of the source action (parameters are stored as fields); anything else is a load error, so a string written without its single quotes is caught.

If a named field is missing on the source entity at dispatch time, the key is logged (`tracing::warn!`) and skipped; the reaction still fires with the other params.

### `resolve_target` — how to pick the target entity ID

| `kind` | Required fields | Behavior |
|---|---|---|
| `field` | `id_field` | Read the target entity ID from a source field. Missing → reaction skipped (warn). |
| `same_id` | — | Target ID = source entity ID. |
| `static` | `entity_id` | Target ID is a fixed string. Useful for per-tenant singletons. |
| `create_if_missing` | `id_field` | Read target ID from source field; if absent, derive `"{source_id}-derived"`. Good for per-source-entity singletons (e.g., one `FileVersion` per `File`). |
| `create` | — | Fresh UUID on every dispatch via `sim_uuid()`. Correct choice for pipeline chaining where each source action spawns a brand-new target instance. |

---

## Guards

`guard` is an optional predicate, in the same grammar as every spec condition ([predicates.md](predicates.md)), that gates firing. It reads the source entity's fields and post-action `status`, and related entities' statuses. Guard-skipped rules do **not** emit a `ReactionResult` — they never fired.

```toml
[[action.triggers]]
name = "complete_ranks_session"
kind = "entity"
guard = "ready && job_type in ['rank', 'source_search'] && Workspace[workspace_id].status == 'Active'"
target_entity = "CurationJob"
target_action = "Submit"
resolve_target = { kind = "create" }
```

- `status == 'S'` and `status in [...]` conjuncts also tell the registry and the trigger graph which source states the trigger can fire in.
- Guards apply to entity triggers only; a `guard` on a wasm, adapter, webhook or hook trigger is a load error (those dispatchers do not evaluate one).

- A missing source field reads as `null`: `ready` is false, and `ready == false` is false too.
- `Workspace[workspace_id].status` reads `workspace_id` from the source entity and fetches each referenced entity's status via `resolve_entity_status` (the same path action guards use). An unset id or a missing entity reads as `null`, so `== 'Active'` is false.

---

## Three example patterns

### 1. Pipeline chaining

Each `CurationJob` completion spawns the next stage as a new job (on `CurationJob`'s `Complete` action):

```toml
[[action.triggers]]
name = "source_search_complete_triggers_rank"
kind = "entity"
guard = "job_type == 'source_search'"
target_entity = "CurationJob"
target_action = "Submit"
args = { job_type = "'rank'", input = "output" }
resolve_target = { kind = "create" }
```

Fresh UUID for the new job, `output` from the source piped into the target's `input`.

### 2. Session-completion callback

When a child `Session` completes, Ack the parent — but only if the parent is still Active (on `Session`'s `Complete` action):

```toml
[[action.triggers]]
name = "session_complete_acks_parent"
kind = "entity"
guard = "Workspace[workspace_id].status == 'Active'"
target_entity = "Workspace"
target_action = "AckSession"
args = { session_id = "Id" }
resolve_target = { kind = "field", id_field = "workspace_id" }
```

### 3. Cleanup-on-failed

When an entity enters Failed, clean up its related resources (on `Order`'s `FailOrder` action):

```toml
[[action.triggers]]
name = "order_failed_releases_inventory"
kind = "entity"
guard = "status == 'Failed'"
target_entity = "InventoryHold"
target_action = "Release"
args = { order_id = "Id" }
resolve_target = { kind = "field", id_field = "hold_id" }
```

---

## Invariants (what does NOT change)

These properties are load-bearing and unchanged by any of the four Phase additions:

- **Fire-and-forget.** A failing reaction does NOT roll back the source transition. The source action is already committed by the time the dispatcher runs. A target that refuses the move leaves a `reaction_refused` event on the source record's observe stream and a `reaction refused` warning in the log, each with the rule, the target, its state and why.
- **Cascade bound.** A chain of reactions runs as long as its steps differ. The first reaction that would repeat a step the chain is already in (the same action on the same entity; for an entity the chain made, the same action on that type) is not run. It comes back as a failed `ReactionResult` naming the loop, and the entity whose action would have fired it gets a `reaction_cycle_stopped` observe event. A child made by `spawn(...)` is an entity the chain made, and its initial action is the chain's next step: a spawn that would repeat a step is refused before the child is made, with an `integration_callback_rejected` event on the parent naming the loop. To repeat work on purpose, use a state timer: a timer starts a new chain.
- **Tenant isolation.** Reactions only fire for rules registered under the same tenant as the source action.
- **Cedar on every dispatch.** Target actions run under the invoking principal, or under the service identity named by the trigger's `principal`. There is no system-principal bypass.
- **Determinism under `SimReactionSystem`.** Two seeded runs with the same inputs produce the same reaction firing order and the same `create`-resolver IDs.
- **Guard nesting bound.** The predicate parser caps nesting at `MAX_DEPTH = 64` levels.

A tenant's reaction-rule count is deliberately **not** on this list. `MAX_REACTIONS_PER_TENANT = 256` is an advisory threshold: `register_tenant_rules` warns above it and registers every rule. It asserted until 2026-09-10, when a tenant's fifteenth app took it to 265 rules and the panic crash-looped the platform at startup. Nothing is sized from it, so exceeding it corrupts nothing. See ADR-0176.

---

## Where reactions fit in the Temper architecture

Reactions are the *composition* layer. Actions are the *contract* layer.

- **Actions** are the entity's verified surface — preconditions, guards, effects, invariants. Each entity is a closed I/O Automaton, which is why `temper-verify` can model-check each action in isolation.
- **Reactions** are how apps wire entities together. They fire *after* actions commit, authorized by Cedar, non-transactional, bounded. Effects never reach another entity; only triggers do.

Keeping the layers separate is what makes verification tractable (each entity stays a closed automaton), authorization coherent (a trigger that needs more authority than its caller names a service `principal`), and cascades bounded. See [ADR-0045 Sub-Decision 5](adrs/0045-reactions-first-class-app-primitive.md#sub-decision-5-keep-reactions-separate-from-actions-architectural-reaffirmation) for the full rationale.

`os-apps/temper-fs/specs/file.ioa.toml` and `file_version.ioa.toml` are production examples. Apps that need cross-entity choreography should prefer entity triggers over WASM triggers unless they need computation, external I/O, or retries.

## Converting a `reactions.toml` rule

Each `[[reaction]]` becomes an entity trigger on the action named in `[reaction.when]`: `[reaction.then] entity_type`/`action` become `target_entity`/`target_action`; `to_state` becomes a `status == '...'` conjunct of `guard`; `params` (literals, in single quotes) and `params_from` (field names) become `args`; and `resolve_target` is tagged with `kind`.

```toml
# before (reactions.toml)
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"

# after: on Order's ConfirmOrder action
[[action.triggers]]
name = "order_confirmed_authorizes_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"
resolve_target = { kind = "same_id" }
```
