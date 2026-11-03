# Predicates

Every condition in an IOA spec is one string in one grammar (ADR-0179):

| Where | Key | Reads |
|---|---|---|
| `[[action]]` | `guard` | the entity before the action |
| `[[action.triggers]]` | `guard` | the entity after the action |
| `[[invariant]]` | `assert` | every reachable state (proven by the verification cascade), and the entity before and after every move ([checked on moves](#invariants-on-every-move)) |
| `[[field_invariant]]` | `assert` | the entity's fields after a write (checked on writes) |

An action's `effect` is a list of statements over the same literals and names ([Effects](#effects)), and trigger and state-timeout `args` are values in it ([Arguments](#arguments)).

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
atom    = "(" expr ")" | "empty" "(" name ")" | name | "params" "." name | operand cmp operand
        | operand [ "not" ] "in" ( "[" literal, ... "]" | name )
cmp     = "==" | "!=" | "<" | "<=" | ">" | ">="
operand = "status" | name | "params" "." name | "len" "(" name ")" | Type "[" name "]" "." "status" | literal
literal = integer | 'string' | true | false | null
```

- **Strings use single quotes**, so an expression sits inside an ordinary TOML `"..."` string. Double an apostrophe inside a literal: `'owner''s'`.
- **`status`** is the entity's status. **Bare names** are declared state variables in action guards and invariants (plus reference fields inside `empty(...)` and `Type[...]`), and any entity field in trigger guards and field invariants.
- **A bare name is a boolean test**: `ready`, `!ready`. Anything else must be compared.
- **`null` means absent**: `x == null`. **`empty(x)`** is absent, null, `''` or `[]`.
- **`len(list)`** and **`'v' in list`** read list state variables.
- **`Type[ref].status`** is the status of the related `Type` entity whose id is in field `ref` (one id, or a list of ids). An unset reference, or one whose entity is not found, reads as `null`. A comparison must hold for **every** related entity.
  - required: `Workspace[workspace_id].status in ['Active']`
  - optional: `empty(workspace_id) || Workspace[workspace_id].status in ['Active']`
  - only a bad state blocks: `Workspace[workspace_id].status not in ['Frozen', 'Archived']`
- **`params.p`** is the parameter `p` of the move being taken. Only an action's guard reads one, and only a parameter its action declares. A bare `params.p` is a boolean test, like a bare name, and prints as `params.p == true`. That is how the `on_success` actions a module's answer lists choose between its values (see the agent guide).
- **`=>`** is implication: `status in ['Done'] => approved`.

## Effects

`effect` is a list of statements, type-checked when the spec loads (ADR-0180):

```toml
effect = [
  "items += 1",                         # counter add: int, counter var, or params.p
  "retries -= 1",                       # counter subtract, stops at 0
  "quota_limit = params.quota_limit",   # counter set
  "error = ''",                         # string set: a literal, including empty
  "ready = true",                       # bool set: true/false, bool var, or params.p
  "append(tags, params.tag)",           # list append: 'string' or params.p
  "remove_at(tags, params.tag_index)",  # list remove by index; out of range is a no-op
  "remove(tags, params.tag)",           # list remove by value: every copy; absent is a no-op
  "schedule('Expire', 3600)",           # dispatch an action on this entity after N seconds
  "schedule_at('Expire', expires_at)",  # dispatch at the timestamp in a field
  "spawn('Task', 'Create', last_task_id)",  # create a child, run 'Create' on it, store its id
]
```

```text
effect  = assign | call
assign  = name ( "=" | "+=" | "-=" ) arg
call    = "append" "(" name "," arg ")" | "remove" "(" name "," arg ")"
        | "remove_at" "(" name "," arg ")"
        | "schedule" "(" 'action' "," integer ")" | "schedule_at" "(" 'action' "," name ")"
        | "spawn" "(" 'type' "," 'action' [ "," name [ "," arg ] ] ")"
arg     = integer | 'string' | true | false | name | "params" "." name
```

- **`params.p`** is the action parameter `p`.
- **Counters, bools and strings are assigned**; a string accepts only `=` with a string literal, and its target must be a declared string state variable. String copies, expressions and `params.p` assignments are refused at load. `+=` and `-=` apply only to counters. Lists take `append`/`remove`/`remove_at`, and their elements are strings. To keep a list as a set, guard `append` with `params.p not in list`; `remove` takes out every copy of a value, so two removes of different values never take out the wrong one.
- **`schedule`/`schedule_at` targets** must be declared actions.
- **`spawn`** creates a child with a fresh id, or the id in its optional fourth argument (`'string'` or `params.p`). The child's initial action receives the parent action's params plus `parent_type`, `parent_id` and `<parent>_id`.
- **Nothing is implied by an action's name.** An action with no `effect` changes only `status`.
- **Work on other entities, modules, adapters, webhooks and platform hooks** is an `[[action.triggers]]` entry, not an effect ([reactions.md](reactions.md)).

## Arguments

`[[action.triggers]]` and `[[state_timeout]]` pass parameters to the action they dispatch with one `args` table (ADR-0181). Each value is a string holding a literal or a name:

```toml
args = { source = "'doc'", attempts = "3", urgent = "true", reason = "null", doc_id = "Id", owner = "owner_id" }
```

- **Literals:** `'text'` in single quotes (double an apostrophe as `''`), an integer, `true`, `false`, `null`.
- **Names:** `Id` is the entity's id; any other name is that field, read after the source action commits (triggers) or when the timer is armed (timeouts). A trigger may name a declared state variable or a parameter of its action; a timeout a declared state variable. Anything else fails to load, so a string written without its single quotes is caught.
- **A missing field** leaves its key out of the dispatched params (logged).

## Terminal states

States no action may leave are listed on the automaton, not written as an invariant:

```toml
[automaton]
states = ["Draft", "Done", "Cancelled"]
initial = "Draft"
terminal = ["Done", "Cancelled"]
```

## What the verifier can prove

`[[invariant]]` asserts are proven over every reachable state. They may read `status`, `counter`, `bool` and `string` state variables, the length and members of `list` variables, and a related entity's status or field. An assertion that reads a number that is not a counter, an undeclared field, or `null` fails to load; state it as a `[[field_invariant]]` instead, which is checked whenever the entity is written.

String fields read by a guard or invariant, or written by an effect, are tracked. An effect-only string ranges over its initial value and the finite set of literals effects write. A field also written by action params keeps the existing equality-symbol abstraction for arbitrary input.

## Invariants on every move

The kernel also checks a type's `[[invariant]]`s on every move, whatever made it: a request, a timer, a reaction or a module's answer. A move that makes an invariant false when it held before the move is refused with `InvariantViolated`, which names the invariant (HTTP 409), and the entity is left as it was. An entity that already breaks an invariant, such as one written before the invariant was added, can still move, and the kernel logs a warning naming the invariant.

A related entity's status (`Type[ref].status`) is read as the move resolved it, and a move that changes `ref` is not checked against that invariant. An invariant that reads a related entity's field (`Type[ref].field`) is not checked on moves; the kernel logs which ones when it loads the spec.

Guards may also read values the model does not track (reference fields, related entities). The verifier treats those as unknown: an action gated on one may fire, but is not guaranteed to.

Effects that read `params.p` are verified with `p` unknown: a counter takes every value in `0..=bound`, a bool both values, and a list element every string literal a guard or invariant compares that list against, plus one fresh value.

## Converting old specs

Specs written before this grammar (guard tables and clauses such as `is_true x` or `min x 3`, `when` + `assert`, `no_further_transitions`, trigger-guard and field-predicate tables, table or verb-string effects such as `{ type = "set_string", var = "error", value = "" }`, `{ type = "increment", var = "items" }` or `"set ready true"`, `trigger`/`emit` effects and `[[integration]]` blocks) fail to load with a hint. Convert them in place:

```bash
temper migrate-predicates path/to/*.ioa.toml
```

The converter rewrites effects as statements, moves integrations onto the actions that triggered them as `[[action.triggers]]`, writes out the effects the old action-name heuristic implied (`AddItem` incrementing counters), and prints a note for anything it drops: `emit` effects, integrations no action triggered, and webhook integrations without a `url` (which never fired).

It also converts the value spellings and key names that preceded strict typed reading (ADR-0181): string booleans and numbers (`"true"`, `"0"`) become TOML values; `[[state]] initial` is written in its type; the removed type names map to `string`, `bool`, `counter`, `int` and `list`; `Composite` becomes `composite`; trigger `to_state` becomes a `status == '...'` conjunct of `guard`; `params`/`params_from` become `args`; `resolve_target` `type`/`field` become `kind`/`id_field`; webhook `entity_lookup`/`entity_param` become `entity_id = "query.<name>"`; `adapter_type` becomes `adapter`. Keys the old reader ignored — unknown keys and sections, `[automaton.timeouts]`, keys of another trigger kind (such as a `guard` on a wasm trigger), composite-only keys on other actions — are dropped with a note, so the converted spec means what the old one did.
