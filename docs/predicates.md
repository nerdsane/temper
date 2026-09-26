# Predicates

Every condition in an IOA spec is one string in one grammar (ADR-0179):

| Where | Key | Reads |
|---|---|---|
| `[[action]]` | `guard` | the entity before the action |
| `[[action.triggers]]` | `guard` | the entity after the action |
| `[[invariant]]` | `assert` | every reachable state (proven by the verification cascade) |
| `[[field_invariant]]` | `assert` | the entity's fields after a write (checked on writes) |

An action's `effect` is a list of statements over the same literals and names ([Effects](#effects)).

```toml
[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > 0 && has_address"

[[invariant]]
name = "ShippedHasItems"
assert = "status in ['Shipped', 'Delivered'] => items > 0"

[[field_invariant]]
name = "ExternalNeedsUrl"
assert = "kind == 'external' => !empty(url)"
message = "external links need a url"
```

## Grammar

```text
expr    = implies
implies = or [ "=>" implies ]          lowest precedence, right-associative
or      = and { "||" and }
and     = unary { "&&" unary }
unary   = "!" unary | atom
atom    = "(" expr ")" | "empty" "(" name ")" | name | operand cmp operand
        | operand [ "not" ] "in" ( "[" literal, ... "]" | name )
cmp     = "==" | "!=" | "<" | "<=" | ">" | ">="
operand = "status" | name | "len" "(" name ")" | Type "[" name "]" "." "status" | literal
literal = integer | 'string' | true | false | null
```

- **Strings use single quotes**, so an expression sits inside an ordinary TOML `"..."` string.
- **`status`** is the entity's status. **Bare names** are declared state variables in action guards and invariants (plus reference fields inside `empty(...)` and `Type[...]`), and any entity field in trigger guards and field invariants.
- **A bare name is a boolean test**: `ready`, `!ready`. Anything else must be compared.
- **`null` means absent**: `x == null`. **`empty(x)`** is absent, null, `''` or `[]`.
- **`len(list)`** and **`'v' in list`** read list state variables.
- **`Type[ref].status`** is the status of the related `Type` entity whose id is in field `ref` (one id, or a list of ids). An unset reference, or one whose entity is not found, reads as `null`. A comparison must hold for **every** related entity.
  - required: `Workspace[workspace_id].status in ['Active']`
  - optional: `empty(workspace_id) || Workspace[workspace_id].status in ['Active']`
  - only a bad state blocks: `Workspace[workspace_id].status not in ['Frozen', 'Archived']`
- **`=>`** is implication: `status in ['Done'] => approved`.

## Effects

`effect` is a list of statements, type-checked when the spec loads (ADR-0180):

```toml
effect = [
  "items += 1",                         # counter add: int, counter var, or params.p
  "retries -= 1",                       # counter subtract, stops at 0
  "quota_limit = params.quota_limit",   # counter set
  "ready = true",                       # bool set: true/false, bool var, or params.p
  "append(tags, params.tag)",           # list append: 'string' or params.p
  "remove_at(tags, params.tag_index)",  # list remove by index; out of range is a no-op
  "schedule('Expire', 3600)",           # dispatch an action on this entity after N seconds
  "schedule_at('Expire', expires_at)",  # dispatch at the timestamp in a field
  "spawn('Task', 'Create', last_task_id)",  # create a child, run 'Create' on it, store its id
]
```

```text
effect  = assign | call
assign  = name ( "=" | "+=" | "-=" ) arg
call    = "append" "(" name "," arg ")" | "remove_at" "(" name "," arg ")"
        | "schedule" "(" 'action' "," integer ")" | "schedule_at" "(" 'action' "," name ")"
        | "spawn" "(" 'type' "," 'action' [ "," name [ "," arg ] ] ")"
arg     = integer | 'string' | true | false | name | "params" "." name
```

- **`params.p`** is the action parameter `p`.
- **Only counters and bools are assigned**; `+=` and `-=` only on counters. Lists take `append`/`remove_at`, and their elements are strings.
- **`schedule`/`schedule_at` targets** must be declared actions.
- **`spawn`** creates a child with a fresh id, or the id in its optional fourth argument (`'string'` or `params.p`). The child's initial action receives the parent action's params plus `parent_type`, `parent_id` and `<parent>_id`.
- **Nothing is implied by an action's name.** An action with no `effect` changes only `status`.
- **Work on other entities, modules, adapters, webhooks and platform hooks** is an `[[action.triggers]]` entry, not an effect ([reactions.md](reactions.md)).

## Terminal states

States no action may leave are listed on the automaton, not written as an invariant:

```toml
[automaton]
states = ["Draft", "Done", "Cancelled"]
initial = "Draft"
terminal = ["Done", "Cancelled"]
```

## What the verifier can prove

`[[invariant]]` asserts are proven over every reachable state, so they may only read what the model tracks: `status`, `counter`, `bool` and `list`/`set` state variables. An assertion over a string or number variable, an entity field, or a related entity fails to load; state it as a `[[field_invariant]]` instead, which is checked whenever the entity is written.

Guards may also read values the model does not track (string variables, reference fields, related entities). The verifier treats those as unknown: an action gated on one may fire, but is not guaranteed to.

Effects that read `params.p` are verified with `p` unknown: a counter takes every value in `0..=bound`, a bool both values, and a list element every string literal a guard or invariant compares that list against, plus one fresh value.

## Converting old specs

Specs written before this grammar (guard tables and clauses such as `is_true x` or `min x 3`, `when` + `assert`, `no_further_transitions`, trigger-guard and field-predicate tables, table or verb-string effects such as `{ type = "increment", var = "items" }` or `"set ready true"`, `trigger`/`emit` effects and `[[integration]]` blocks) fail to load with a hint. Convert them in place:

```bash
temper migrate-predicates path/to/*.ioa.toml
```

The converter rewrites effects as statements, moves integrations onto the actions that triggered them as `[[action.triggers]]`, writes out the effects the old action-name heuristic implied (`AddItem` incrementing counters), and prints a note for anything it drops: `emit` effects, integrations no action triggered, and webhook integrations without a `url` (which never fired).
