# Predicates

Every condition in an IOA spec is one string in one grammar (ADR-0179):

| Where | Key | Reads |
|---|---|---|
| `[[action]]` | `guard` | the entity before the action |
| `[[action.triggers]]` | `guard` | the entity after the action |
| `[[invariant]]` | `assert` | every reachable state (proven by the verification cascade) |
| `[[field_invariant]]` | `assert` | the entity's fields after a write (checked on writes) |

`reactions.toml` `[reaction.when] guard` uses the same grammar as trigger guards.

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

## Converting old specs

Specs written before this grammar (guard tables and clauses such as `is_true x` or `min x 3`, `when` + `assert`, `no_further_transitions`, trigger-guard and field-predicate tables) fail to load with a hint. Convert them in place:

```bash
temper migrate-predicates path/to/*.ioa.toml
```
