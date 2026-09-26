# ADR-0180: Effects as statements; triggers as the only outgoing call

- Status: Accepted
- Date: 2026-09-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0179: One predicate grammar for every spec condition (the grammar effects extend)
  - ADR-0046: Unified action triggers (`[[action.triggers]]`, now the only outgoing call)
  - ADR-0078: Inline action trigger adapters
  - ADR-0041: Governance decision callbacks (the `DispatchCallback` hook)
  - ADR-0150: Composite cross-entity verification
  - Issue #500 (audit section C, proposal and decisions)
  - `crates/temper-spec/src/predicate/effect.rs`, `crates/temper-spec/src/automaton/translate.rs`

## Context

After ADR-0179 every spec condition used one grammar, but what an action *does* still had two syntaxes and several side channels:

1. **Two effect syntaxes, used about equally:** verb strings (`"increment items"`, `"set ready true"`) and `{ type = ... }` tables. Only the tables had `schedule`, `spawn`, list and param effects; `schedule_at` took its arguments in opposite orders in the two forms. Aliases (`emit`/`emit_event`, `spawn`/`spawn_entity`, `var`/`list`) and silent drops (a table missing `var` was discarded) made both lenient.
2. **Implicit parameter names:** `list_append x` appended the parameter named `x`; `list_remove_at x` read `x_index`.
3. **Effects from action names:** an action named like `AddItem` with no effect incremented every counter.
4. **Unverified parameter effects:** `increment x by p` and `set_counter_from_param` were "runtime-only"; the verifier assumed the counter did not move, so invariants over those counters were proven against a model that never changed them (the temper-fs quota was one).
5. **Four ways to cause work elsewhere:** `[[integration]]` blocks matched by name from a `trigger` effect, `emit` effects routed by `reactions.toml` (only in the actor runtime; the default runtime logged them), `reactions.toml` rules, and `[[action.triggers]]`. Platform hooks were `trigger` effects whose names happened to be capitalized.
6. **Four effect representations downstream** (spec, resolved, JIT, verifier), with `items` special cases.

## Decision

### Sub-Decision 1: `effect` is a list of statements in the predicate grammar

```toml
effect = ["items += 1", "used_bytes += params.size_bytes", "ready = true",
          "append(tags, params.tag)", "remove_at(tags, 0)",
          "schedule('Expire', 3600)", "schedule_at('Expire', expires_at)",
          "spawn('Task', 'Create', last_task_id)"]
```

```ebnf
effect = assign | call ;
assign = name ( "=" | "+=" | "-=" ) arg ;
call   = "append" "(" name "," arg ")" | "remove_at" "(" name "," arg ")"
       | "schedule" "(" 'action' "," int ")" | "schedule_at" "(" 'action' "," name ")"
       | "spawn" "(" 'type' "," 'action' [ "," name [ "," arg ] ] ")" ;
arg    = int | 'string' | true | false | name | "params" "." name ;
```

Literals and names are ADR-0179's; `params.p` is the one addition and is legal only in effects (`params` is reserved in predicates). Statements are checked at load against the declared state: only counters and bools are assigned, `+=`/`-=` need a counter, list elements are strings, a parameter is read as one kind only, and `schedule` targets must exist. `-=` stops at 0 and `remove_at` out of range is a no-op, as before.

**Why:** one form per effect, nothing implicit, and the same checker discipline as guards. A list reads and diffs better than one `;`-joined string for long effect lists.

### Sub-Decision 2: `spawn` stays an effect

A spawned child is created in the parent's transition (its id is stored in the parent in the same commit), so it is state, not an outgoing call. The child id is fresh unless a fourth argument (`'string'` or `params.p`) supplies one, which keeps caller-chosen, idempotent child ids. `copy_fields` (unused) is gone; the initial action receives the parent's params plus `parent_type`, `parent_id` and `<parent>_id` as before.

### Sub-Decision 3: parameter-driven effects are modeled

`params.p` is an unknown value to every verification level. Stateright, simulation and proptest choose it per step (`TemperModelAction::params`): counts range over `0..=bound`, booleans over both values, list elements over every literal a guard or invariant compares the list against plus one fresh string. SMT encodes it as a fresh non-negative integer or boolean. A counter's exploration bound is raised to the largest literal written to it so literal writes are not pruned.

**Why:** an invariant over a parameter-driven counter was previously "proven" against a counter that never moved. This enlarges the state space only for actions that read parameters.

### Sub-Decision 4: `[[action.triggers]]` is the only way an action causes work elsewhere

Retired: `[[integration]]`, the `trigger` effect, the `emit` effect and `reactions.toml`. Each action still emits its own name implicitly, which is what webhooks and triggers key on. Platform hooks are a new trigger kind, `kind = "hook"` with `hook = "DispatchCallback"`. Internally the parser still derives the WASM/adapter dispatch records (`automaton.integrations`) from external triggers, and translation emits one dispatch per external or hook trigger. The actor runtime (`temper-actor-runtime`) routes an accepted action to sibling actors per the spec's `same_id` entity triggers instead of reaction rules. The registry API loses its reaction-rules parameter.

**Why:** one place to declare, validate, verify (the composite verifier and trigger graph already read triggers) and read outgoing calls. `emit` had no consumer outside the actor runtime's reaction routing.

### Sub-Decision 5: no effects from action names; one resolved form

The `AddItem`/`RemoveItem` heuristic is gone. `ResolvedEffect` (counter set/add/sub, bool set, list append/remove, dispatch, schedule, schedule_at, spawn) is the single intermediate: the JIT mirrors it with a status change, the verifier keeps its state effects, and the `IncrementItems`/`DecrementItems`/`EmitEvent` JIT variants are gone. Runtimes resolve values with `temper_jit::table::effect_args`, so a literal, a variable and `params.p` mean the same thing in the default runtime and the actor runtime.

## Rollout Plan

1. **This change:** grammar, consumers, verifier modeling, retired syntax rejected at load with a pointer to `temper migrate-predicates`, which converts old effects, `[[integration]]` blocks and `trigger`/`emit` effects in place (copying a reused trigger onto each action that fired it, and writing the effects the name heuristic implied). Every spec in the repo is converted.
2. **Follow-up:** trigger `guard`/`to_state` on external (wasm/adapter/webhook) triggers are still not evaluated before dispatch; webhook triggers still have no dispatcher (ADR-0046 known gap).

## Consequences

### Positive
- One grammar for conditions and state changes; effects are checked at load.
- Parameter-driven invariants are verified rather than assumed.
- Outgoing calls are declared, validated and verified in one place.

### Negative
- Breaking: specs, `reactions.toml` files and apps outside this repo must be converted (`temper migrate-predicates` for specs; reaction rules become entity triggers by hand).
- Label-only `emit` events (`PolicyActivated`, ...) no longer appear in logs.

### Risks
- Larger state spaces for actions that read parameters. Mitigation: bounded domains and a per-transition assignment budget (`MAX_PARAM_ASSIGNMENTS`).
- The composite verifier's cascade takes the first enabled parameter choice for a triggered action.

### DST Compliance
- Spawned ids still come from `sim_uuid()`; parameter choices are enumerated deterministically (`BTreeMap` order).

## Non-Goals

- Merging `[[action.constraints]]` into guards, and `[[cross_invariant]]` (ADR-0179 non-goals).
- Named arguments for calls.

## Alternatives Considered

1. **One `;`-separated effect string** — matches `guard` exactly, but long effect lists read and diff worse.
2. **Spawn as an entity trigger with a `create` resolver** — a spawn writes the child id into the parent in the same commit, which a post-commit trigger cannot.
3. **Keep ignoring parameter effects** — keeps state spaces small, but leaves invariants over those counters unproven.
4. **Keep `[[integration]]` for a later change** — would have kept the `trigger` effect and the name-matching dispatch alive.
