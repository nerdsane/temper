# Temper App Builder

**You MUST use this skill when the user asks to build an app, create an application, or says "build me a X".** Do NOT use other skills for app creation. Temper builds apps from verified specs, not from code. Interview FIRST, CLI LATER.

## Interview Protocol

When the user says "build me a X", run steps 1-6 in conversation. Do NOT run any CLI commands until the interview is complete.

### Step 1: Identify Entities
Ask: "What are the main things (entities) in your system?" Guide them toward nouns (Users, Orders, Tasks). Each entity becomes a `.ioa.toml` spec file.

### Step 2: Define States
Ask: "What states can a [Entity] be in?" Keep it to 3-5 states. States must be mutually exclusive and exhaustive.

### Step 3: Define Actions
Ask: "What actions move a [Entity] between states?" Each action is a verb (Create, Submit, Approve). Actions specify `from` (array of states) and `to` (target state).

### Step 4: Define Guards
Ask: "Are there conditions for an action to be allowed?" Guards are string expressions: `guard = "items > 0"`, `guard = "is_true has_reviewer"`. Operators: `>`, `<`, `>=`, `<=`, `==`, `is_true`, `min`.

### Step 5: Define Effects
Ask: "What side effects happen during a transition?" Effects modify state variables: `effect = "increment counter"`, `effect = "set flag true"`, `effect = "decrement count"`.

### Step 6: Define Invariants
Ask: "What rules must ALWAYS be true?" Format: `when` (states), `assert` (expression). Common: `"items > 0"`, `"no_further_transitions"` (terminal state).

## Generate Specs

Verify the `temper` CLI is available:
```bash
temper --help
```
If missing: `cargo install --git https://github.com/nerdsane/temper temper-cli`.

Run `temper init <project-name>` in the **user's working directory** (not the temper repo). Then generate specs directly in `specs/` — flat layout, no subdirectories (the CLI does not recurse).

### Write IOA Specs

Use the Write tool to create `specs/<entity>.ioa.toml` for each entity. Use this template:

```toml
[automaton]
name = "EntityName"
states = ["State1", "State2", "State3"]
initial = "State1"

[[state]]
name = "counter_var"
type = "counter"        # "counter" | "bool"
initial = "0"

[[action]]
name = "ActionName"
kind = "input"          # "input" | "output" | "internal"
from = ["State1"]
to = "State2"
guard = "counter_var > 0"
effect = "increment counter_var"
params = ["Param1"]
hint = "Description for agents."

[[invariant]]
name = "InvariantName"
when = ["State3"]
assert = "no_further_transitions"

[[liveness]]
name = "EventuallyDone"
from = ["State1"]
reaches = ["State3"]

[[integration]]
name = "notify_service"
trigger = "ActionName"
type = "webhook"
```

### Write CSDL

Use the Write tool to create `specs/model.csdl.xml`. The CSDL must exactly match the IOA specs. Follow the mapping rules in the Reference section below.

**Checklist before writing CSDL:**
- [ ] Every state in `[automaton].states` appears in `StateMachine.States` Collection
- [ ] `[automaton].initial` matches `StateMachine.InitialState`
- [ ] `Spec` annotation points to the `.ioa.toml` filename
- [ ] Every `kind = "input"` or `kind = "internal"` action has a matching `<Action>`
- [ ] Every action's `from` matches `ValidFromStates`; every `to` matches `TargetState` (omit for self-loops)
- [ ] Every action's `params` become `<Parameter>` elements (after `bindingParameter`)
- [ ] `kind = "output"` actions do NOT appear in CSDL
- [ ] All `<Action>` entries have `IsBound="true"` with correct `bindingParameter` type
- [ ] EntityContainer has an `<EntitySet>` for each EntityType

### Write Cedar Policies (if needed)

Place Cedar policies in `specs/policies/`. Example:
```cedar
permit(principal is User, action == Action::"read", resource is Order)
when { resource.ownerId == principal.id };
```

## Start Server (Before Specs)

**ALWAYS use port 3333.** Check if a server is already running before starting a new one:

```bash
curl -s http://localhost:3333/observe/health
```

- **If it returns JSON** (server already running): Skip starting. Tell the user: "Temper server already running on port 3333. Your app will be added as a new tenant."
- **If it fails** (no server): Start one:
  ```bash
  temper serve --port 3333 &
  ```

Tell the user: "Open http://localhost:3001 to watch specs load and verify in real-time."

For persistence, set `DATABASE_URL`:
```bash
DATABASE_URL=postgres://user:pass@localhost:5432/dbname temper serve --port 3333 &
```

**IMPORTANT:** Never start on a different port. All apps share one server as multi-tenant. This ensures `/temper-user` can always find them.

## Push Specs (After Writing)

After writing all spec files to `specs/`, push them to the running server:
```bash
curl -s -X POST http://localhost:3333/observe/specs/load-dir \
  -H "Content-Type: application/json" \
  -d '{"tenant":"<app-name>","specs_dir":"./specs"}'
```

This triggers the full verification cascade (L0-L3) in the background. The Observe UI at http://localhost:3001 shows:
- SpecCards appearing for each entity (fade in)
- Verification dots pulsing amber (running) then flashing teal (passed)
- Progress bar filling as each level completes
- "All entities verified" confirmation

If any verification level fails, translate the error using the error table in the Reference section. Fix specs, then push again.

## Confirm Ready

After pushing specs, watch the Observe UI for verification to complete. Tell the user the app is live and show them how to interact:
```bash
# List entities
curl http://localhost:3333/tdata/Orders

# Create entity
curl -X POST http://localhost:3333/tdata/Orders \
  -H "Content-Type: application/json" -d '{"title": "My Order"}'

# Invoke action
curl -X POST http://localhost:3333/tdata/Orders\('entity-id'\)/Ns.SubmitOrder \
  -H "Content-Type: application/json" -d '{}'

# Get single entity
curl http://localhost:3333/tdata/Orders\('entity-id'\)
```

The API is at `/tdata` (not `/odata`). Invalid actions return 409 Conflict.

## Handle Changes

When the user wants to modify the app, edit the spec files directly with the Edit tool, then push again:
```bash
curl -s -X POST http://localhost:3333/observe/specs/load-dir \
  -H "Content-Type: application/json" \
  -d '{"tenant":"<app-name>","specs_dir":"./specs"}'
```

## Check Unmet Intents

When the user asks about feedback or what users want:
```bash
curl http://127.0.0.1:3333/observe/trajectories | jq '.failed_intents'
```
Analyze patterns, propose spec changes, get developer approval, modify specs, re-verify, restart.

---

## Reference

### IOA TOML Spec Format

```toml
[automaton]
name = "EntityName"
states = ["State1", "State2", "State3"]
initial = "State1"

# State variables
[[state]]
name = "counter_var"
type = "counter"
initial = "0"

[[state]]
name = "bool_var"
type = "bool"
initial = "false"

# Actions
[[action]]
name = "ActionName"
kind = "input"             # "input" | "output" | "internal"
from = ["State1"]          # states this action can fire from
to = "State2"              # target state (omit for self-loops)
guard = "counter_var > 0"  # optional guard condition
effect = "increment counter_var" # optional effect
params = ["Param1"]        # optional parameters
hint = "Description."      # optional hint

# Output actions (events, no state change)
[[action]]
name = "SomeEvent"
kind = "output"
hint = "Emitted when something happens."

# Safety invariants
[[invariant]]
name = "InvariantName"
when = ["State2", "State3"]  # states where checked (empty = all)
assert = "counter_var > 0"

# Liveness properties
[[liveness]]
name = "EventuallyResolved"
from = ["State1"]
reaches = ["State3"]

# Integrations
[[integration]]
name = "notify_service"
trigger = "ActionName"
type = "webhook"
```

### IOA-to-CSDL Mapping Rules

**Entity-level:**

| IOA Field | CSDL Annotation | Rule |
|---|---|---|
| `[automaton].name` | `<EntityType Name="...">` | Must be identical |
| `[automaton].states` | `Temper.Vocab.StateMachine.States` | Every state, same order |
| `[automaton].initial` | `Temper.Vocab.StateMachine.InitialState` | Must match exactly |
| (filename) | `Temper.Vocab.StateMachine.Spec` | Points to `.ioa.toml` file |

**Action-level:**

| IOA Field | CSDL Element/Annotation | Rule |
|---|---|---|
| `[[action]].name` | `<Action Name="...">` | Must be identical |
| `[[action]].from` | `ValidFromStates` Collection | Every state in `from` |
| `[[action]].to` | `TargetState` | Must match (omit for self-loops) |
| `[[action]].params` | `<Parameter>` elements | After `bindingParameter` |
| `[[action]].hint` | `Temper.Vocab.Agent.Hint` | Copy verbatim |

**Rules:**
1. Only `kind = "input"` and `kind = "internal"` get CSDL `<Action>` entries. Output actions do NOT.
2. Self-loop actions need `ValidFromStates` but no `TargetState`.
3. Every `<Action>` must have `IsBound="true"` with `bindingParameter` of the entity type.
4. CSDL namespace must be consistent (e.g., `Temper.MyApp`).
5. Map `type = "counter"` to `Edm.Int32`, `type = "bool"` to `Edm.Boolean`.
6. `Spec` annotation must use `Temper.Vocab.StateMachine.Spec` pointing to the IOA file.

### Error Translation Table

| Verification Result | Explanation | Fix |
|---|---|---|
| Dead guard (L0) | Action can never fire — guard always false | Check counter bounds or contradictory conditions |
| Non-inductive invariant (L0) | A transition reaches the state without establishing the condition | Add guard to offending transition or adjust effect |
| Unreachable state (L0) | No action sequence leads to this state | Add missing transition or remove unused state |
| L1 counterexample | Model checker found a specific violating trace | Follow the trace — last transition is the problem |
| L2 simulation failure | Fault injection broke an invariant | Tighten guards or add ordering constraints |
| L3 property test failure | Random action sequence violated invariant | Add guard to prevent the violating action |
| Deadlock (L1) | Entity stuck with no valid actions | Add transition out, or mark terminal with `no_further_transitions` |

### CLI Quick Reference

```bash
temper init <project-name>                    # Scaffold project
temper serve --port 3333 &                    # Start empty server (Observe UI at :3001)
temper verify --specs-dir specs/              # Run verification cascade (L0-L3) locally
# Push specs to running server (triggers verification + SSE events):
curl -s -X POST http://localhost:3333/observe/specs/load-dir \
  -H "Content-Type: application/json" \
  -d '{"tenant":"<name>","specs_dir":"./specs"}'
curl http://localhost:3333/tdata              # Service document
curl http://localhost:3333/tdata/\$metadata   # Full CSDL metadata
curl http://localhost:3333/tdata/Orders       # List entities
curl -X POST http://localhost:3333/tdata/Orders -H "Content-Type: application/json" -d '{}'  # Create
curl -X POST http://localhost:3333/tdata/Orders\('id'\)/Ns.Action -d '{}'  # Invoke action
curl -H "X-Tenant-Id: my-app" http://localhost:3333/tdata/Orders  # Multi-tenant
```

### Directory Layout

```
specs/
  model.csdl.xml        # Exactly this name
  order.ioa.toml        # Flat — no subdirectories
  payment.ioa.toml
  policies/             # Cedar policies (can nest)
```
