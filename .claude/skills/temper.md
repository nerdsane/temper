# Temper App Builder

**You MUST use this skill when the user asks to build an app, create an application, or says "build me a X".** Do NOT use other skills for app creation. Temper builds apps from verified specs, not from code. Interview FIRST, CLI LATER.

## Interview Protocol

When the user says "build me a X", have a quick, friendly conversation to understand what they want. Use plain language — no jargon. Do NOT run any CLI commands until the interview is complete. Keep it to 3 quick questions:

### Step 1: What are you managing?
Ask: "What are the main things you'll be tracking or managing?"

Guide them toward nouns — projects, bugs, orders, people, tickets. Each one becomes an entity. Don't use the word "entity" with them. Say "thing" or use their word for it.

Example: "Sounds like you have **Bugs** and **Developers** — anything else?"

### Step 2: What's the lifecycle?
For each thing, ask: "Walk me through the journey of a [Bug/Order/etc.] from start to finish. What stages does it go through?"

Keep it to 3-6 stages. Use their words. If they say "first it gets reported, then someone picks it up, then they fix it, then it's done" — that's Draft → Open → InProgress → Resolved.

Example: "Got it — so a Bug starts as **Open**, gets **Triaged**, someone **Starts Work**, then it's **Resolved** and finally **Closed**. Sound right?"

### Step 3: What can go wrong or go sideways?
Ask: "Are there any special cases? Things that can be cancelled, restarted, or that shouldn't happen?"

This catches terminal states (Cancelled, Archived), backward transitions (Reopen), and rules (can't ship an empty order). Don't ask about "guards" or "invariants" — listen for "only if", "can't", "must", "not allowed" and translate those into guards/invariants yourself.

Example: "So a Closed bug can't be changed at all — it's final. And you can Cancel from anywhere except Closed. Got it."

### Then summarize and confirm
Before generating anything, summarize in a simple table:

> "Here's what I'll build:
> - **Bug**: Open → Triaged → InProgress → Resolved → Closed (can Cancel from anywhere except Closed)
> - **Developer**: Invited → Active → OnLeave (can Return from leave)
>
> Look good?"

Wait for confirmation. Then generate specs.

### Technical mapping (internal — don't show this to the user)
- Their "stages" = `states` in `[automaton]`
- Their "what you can do" = `[[action]]` entries with `from`/`to`
- "Can't" / "not allowed" = `[[invariant]]` with `assert`
- "Only if" / "must have" = `guard` on the action
- "Final" / "done forever" = `[[invariant]]` with `assert = "no_further_transitions"`
- Counters and flags: infer from context (e.g., "must have items" → counter `items`, guard `items > 0`)

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

### Generate WASM Integration Modules (if app needs external APIs)

When the app needs external API calls (payments, email, notifications, etc.):

1. **Add integration actions to the spec** — use the `trigger` effect pattern:
   ```toml
   [[action]]
   name = "ChargeCard"
   from = ["Pending"]
   to = "Charging"
   effect = "trigger stripe_charge"

   [[action]]
   name = "ChargeSucceeded"
   kind = "input"
   from = ["Charging"]
   to = "Paid"

   [[action]]
   name = "ChargeFailed"
   kind = "input"
   from = ["Charging"]
   to = "PaymentFailed"

   [[integration]]
   name = "stripe_charge"
   trigger = "stripe_charge"
   type = "wasm"
   module = "stripe_charge"
   on_success = "ChargeSucceeded"
   on_failure = "ChargeFailed"
   ```

2. **Generate WASM module source** — create a Rust `cdylib` project under `examples/wasm-modules/`:
   ```bash
   mkdir -p examples/wasm-modules/<module_name>/src
   ```
   - Declare host function externs (`host_log`, `host_get_context`, `host_set_result`, `host_http_call`, `host_get_secret`)
   - Implement `run(ctx_ptr, ctx_len) -> i32` export that: reads context → calls API via `host_http_call` → parses response → sets result
   - Return JSON: `{"action": "CallbackName", "params": {...}, "success": true/false}`
   - See `examples/wasm-modules/echo-integration/` for a complete reference

3. **Compile to WASM**:
   ```bash
   rustup target add wasm32-unknown-unknown  # one-time
   cd examples/wasm-modules/<module_name>
   cargo build --target wasm32-unknown-unknown --release
   ```

4. **Upload module** before deploying the spec:
   ```bash
   curl -X POST http://localhost:3333/observe/wasm/modules/<module_name> \
     -H "X-Tenant-Id: default" \
     -H "Content-Type: application/wasm" \
     --data-binary @target/wasm32-unknown-unknown/release/<module_name>.wasm
   ```

5. **Deploy spec** as usual — the integration will invoke the WASM module when the trigger action fires.

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

## Push Specs and Verify (MANDATORY — DO NOT SKIP)

**You MUST complete ALL steps below. The app is NOT live until the response confirms all entities pass. DO NOT tell the user the app is ready until verification passes. DO NOT move on to showing API examples until verification passes.**

### Push and read results (single command)

The endpoint streams newline-delimited JSON (NDJSON). It loads specs, runs verification inline, and streams each result. **The response does NOT return until all verification is complete.** You do NOT need to poll — just read the output.

**Option A — Local specs directory** (if you have file access to the server machine):
```bash
curl -s -N -X POST http://localhost:3333/observe/specs/load-dir \
  -H "Content-Type: application/json" \
  -d '{"tenant":"<app-name>","specs_dir":"./specs"}'
```

**Option B — Inline specs over HTTP** (if the server is remote):
```bash
curl -s -N -X POST http://localhost:3333/observe/specs/load-inline \
  -H "Content-Type: application/json" \
  -d '{
    "tenant": "<app-name>",
    "specs": {
      "model.csdl.xml": "<full CSDL XML content>",
      "bug.ioa.toml": "<full IOA TOML content>",
      "developer.ioa.toml": "<full IOA TOML content>"
    }
  }'
```

Both endpoints return the same NDJSON streaming response.

The response streams one JSON object per line:

```
{"type":"specs_loaded","tenant":"my-app","entities":["Order","Payment"]}
{"type":"verification_started","entity":"Order"}
{"type":"verification_result","entity":"Order","all_passed":true,"levels":[...]}
{"type":"verification_started","entity":"Payment"}
{"type":"verification_result","entity":"Payment","all_passed":false,"levels":[{"level":"...","passed":false,"summary":"...","details":[{"kind":"liveness_violation","property":"...","description":"..."}]}]}
{"type":"summary","tenant":"my-app","all_passed":false,"entities":{"Order":true,"Payment":false}}
```

### Check the summary line and fix failures (MANDATORY)

**Read the LAST line of the output** — it has `"type":"summary"` with `"all_passed":true/false`.

- **If `"all_passed": true`** → proceed to "App is live" below
- **If `"all_passed": false`**:
  1. Find the `"type":"verification_result"` lines where `"all_passed":false`
  2. Read the `levels[].details` array — it tells you exactly which property failed and why
  3. Map the failure to a fix using the Error Translation Table in the Reference section
  4. Edit the IOA spec to fix the issue
  5. Re-push specs (run the curl command again)
  6. Maximum 3 fix-and-retry cycles, then ask the user for guidance

**CRITICAL: The server returns HTTP 423 Locked for any POST/PATCH/PUT/DELETE on entities that haven't passed verification.** If you skip this step and try to use the API, you will get 423 errors. The 423 response body includes failure details:

```json
{"error": {"code": "VerificationRequired", "message": "...", "details": {"verification_status": "failed", "failed_levels": [...]}}}
```

### App is live (only after all entities pass)

Once all entities pass verification, tell the user the app is live and show them how to interact:
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

The API is at `/tdata` (not `/odata`). Invalid actions return 409 Conflict. Unverified entities return 423 Locked.

## Handle Changes

When the user wants to modify the app, edit the spec files directly with the Edit tool, then push again:
```bash
curl -s -X POST http://localhost:3333/observe/specs/load-dir \
  -H "Content-Type: application/json" \
  -d '{"tenant":"<app-name>","specs_dir":"./specs"}'
```

## Monitor for Unmet Intents (MANDATORY after deployment)

After specs are deployed and verified, you MUST start watching for user failures. This is the evolution loop — users will try actions that don't exist yet, and you detect and fix them.

### Start the watcher

Immediately after successful deployment, start a background polling command using the Bash tool with `run_in_background: true`:

```bash
KNOWN=0; while true; do COUNT=$(curl -s http://localhost:3333/observe/trajectories?success=false 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('error_count',0))" 2>/dev/null || echo 0); if [ "$COUNT" -gt "$KNOWN" ]; then echo "NEW_UNMET_INTENTS: $COUNT (was $KNOWN)"; curl -s http://localhost:3333/observe/trajectories?success=false; exit 0; fi; sleep 10; done
```

This polls every 10 seconds. When a new unmet intent appears, the background task completes and you receive a notification.

### When notified of new unmet intents

1. Read the background task output — it contains the trajectory data with failed intents
2. Tell the user: "I detected an unmet intent: users are trying to **[action]** on **[entity]** but it doesn't exist yet."
3. Propose a fix: explain what transition you'd add (from which states, to which state)
4. Ask for approval: "Should I add this action to the spec?"
5. If approved: edit the spec, re-push, re-verify
6. After fixing, start the watcher again (same command) to catch the next failure

### Manual check

When the user asks about feedback or what users want:
```bash
curl http://localhost:3333/observe/trajectories | jq '.failed_intents'
```

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
# Push specs + verify (streams NDJSON with verification results):
curl -s -N -X POST http://localhost:3333/observe/specs/load-dir \
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
