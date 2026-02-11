# Temper App Builder

Build applications on the Temper platform through conversation. This skill guides you through the interview, spec generation, verification, and deployment workflow.

## Interview Protocol

When the user says they want to build an app (or something like "build me a X"), follow this structured interview:

### Step 1: Identify Entities
Ask: "What are the main things (entities) in your system?"
- Guide them to think about nouns: Users, Orders, Tasks, Projects, etc.
- Each entity becomes a separate `.ioa.toml` spec file and a CSDL EntityType

### Step 2: Define States for Each Entity
Ask: "What states can a [Entity] be in?"
- Start simple: 3-5 states max initially
- Common patterns: Draft/Active/Archived, Open/InProgress/Done, Pending/Approved/Rejected
- States must be mutually exclusive and exhaustive

### Step 3: Define Actions (Transitions)
Ask: "What actions move a [Entity] between states?"
- Each action is a transition: from states (array) to a target state
- Name actions as verbs: Create, Submit, Approve, Archive, Cancel
- Actions can fire from multiple states: `from = ["Draft", "Submitted"]`

### Step 4: Define Guards
Ask: "Are there conditions that must be true for an action to be allowed?"
- Guards are string expressions on state variables
- Format: `guard = "items > 0"` or `guard = "is_true has_reviewer"`
- Guard operators: `>`, `<`, `>=`, `<=`, `==`, `is_true`, `min`

### Step 5: Define Effects
Ask: "What happens when an action is performed (besides the state change)?"
- Effects modify state variables during transitions
- Format: `effect = "increment counter"`, `effect = "set flag true"`, `effect = "decrement count"`
- Effect verbs: `increment`, `decrement`, `set`, `emit`

### Step 6: Define Invariants
Ask: "What rules must ALWAYS be true, regardless of how we got to a state?"
- Invariant format: `name`, `when` (states where checked), `assert` (expression)
- Common assertions: `"items > 0"`, `"no_further_transitions"` (terminal state)

### Step 7: Generate and Verify
1. Run `temper init <project-name>` to scaffold the project (if starting fresh)
2. Generate the `.ioa.toml` spec files in the `specs/` directory
3. Generate matching CSDL in `specs/model.csdl.xml` — **CSDL must exactly match IOA** (see Mapping Rules below)
4. Run `temper verify --specs-dir specs/` to check all specs
5. Translate any verification failures to developer-friendly language
6. Iterate until all cascade levels pass
7. Run `temper serve --specs-dir specs/ --tenant <name>` to deploy

---

## IOA TOML Spec Format (Actual)

This is the exact format parsed by the `temper-spec` crate. Every field matters.

### Minimal Spec

```toml
[automaton]
name = "Item"
states = ["Draft", "Active", "Archived"]
initial = "Draft"

[[action]]
name = "Activate"
kind = "input"
from = ["Draft"]
to = "Active"

[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Archived"
```

### Full Spec Structure

```toml
# Header comment describing the entity
[automaton]
name = "EntityName"
states = ["State1", "State2", "State3"]
initial = "State1"

# --- State Variables ---

[[state]]
name = "counter_var"
type = "counter"
initial = "0"

[[state]]
name = "bool_var"
type = "bool"
initial = "false"

# --- Actions ---

[[action]]
name = "ActionName"
kind = "input"             # "input" | "output" | "internal"
from = ["State1"]          # states this action can fire from (array)
to = "State2"              # target state (optional -- omit for self-loops)
guard = "counter_var > 0"  # optional guard condition (string)
effect = "increment counter_var" # optional effect (string)
params = ["Param1", "Param2"] # optional action parameters (array)
hint = "Description for agents and users." # optional hint (string)

# --- Output Actions (events, no state change) ---

[[action]]
name = "SomeEvent"
kind = "output"
hint = "Emitted when something happens."

# --- Safety Invariants ---

[[invariant]]
name = "InvariantName"
when = ["State2", "State3"]  # states where checked (empty = all)
assert = "counter_var > 0"   # assertion expression

# --- Liveness Properties ---

[[liveness]]
name = "EventuallyResolved"
from = ["State1"]
reaches = ["State3"]

# --- Integrations (external triggers, metadata only) ---

[[integration]]
name = "notify_service"
trigger = "ActionName"
type = "webhook"
```

---

## Spec Patterns Catalog

### Basic Lifecycle

```toml
[automaton]
name = "Item"
states = ["Draft", "Active", "Archived"]
initial = "Draft"

[[action]]
name = "Activate"
kind = "input"
from = ["Draft"]
to = "Active"

[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Archived"

[[invariant]]
name = "ArchivedIsFinal"
when = ["Archived"]
assert = "no_further_transitions"
```

### Approval Workflow

```toml
[automaton]
name = "Approval"
states = ["Drafted", "PendingApproval", "Approved", "Rejected", "Withdrawn"]
initial = "Drafted"

[[state]]
name = "approvals"
type = "counter"
initial = "0"

[[state]]
name = "has_reviewer"
type = "bool"
initial = "false"

[[action]]
name = "AssignReviewer"
kind = "input"
from = ["Drafted"]
to = "Drafted"
effect = "set has_reviewer true"
hint = "Assign a reviewer before submission."

[[action]]
name = "SubmitForApproval"
kind = "internal"
from = ["Drafted"]
to = "PendingApproval"
guard = "is_true has_reviewer"
hint = "Submit for review. Requires an assigned reviewer."

[[action]]
name = "Approve"
kind = "internal"
from = ["PendingApproval"]
to = "Approved"
effect = "increment approvals"

[[action]]
name = "Reject"
kind = "internal"
from = ["PendingApproval"]
to = "Rejected"

[[action]]
name = "Withdraw"
kind = "input"
from = ["Drafted", "PendingApproval"]
to = "Withdrawn"

[[action]]
name = "Revise"
kind = "input"
from = ["Rejected"]
to = "Drafted"
effect = "set has_reviewer false"
hint = "Revise after rejection. Must reassign reviewer."

[[invariant]]
name = "ApprovedHasApproval"
when = ["Approved"]
assert = "approvals > 0"

[[invariant]]
name = "WithdrawnIsFinal"
when = ["Withdrawn"]
assert = "no_further_transitions"
```

### Issue Tracker (Multi-Stage Pipeline)

```toml
[automaton]
name = "Issue"
states = ["Backlog", "InProgress", "Review", "Done", "Archived"]
initial = "Backlog"

[[state]]
name = "assignee_set"
type = "bool"
initial = "false"

[[state]]
name = "review_cycles"
type = "counter"
initial = "0"

[[action]]
name = "AssignIssue"
kind = "input"
from = ["Backlog"]
to = "Backlog"
effect = "set assignee_set true"
hint = "Assign a team member to the issue."

[[action]]
name = "StartWork"
kind = "internal"
from = ["Backlog"]
to = "InProgress"
guard = "is_true assignee_set"
hint = "Begin work. Requires an assignee."

[[action]]
name = "SubmitForReview"
kind = "internal"
from = ["InProgress"]
to = "Review"

[[action]]
name = "RequestChanges"
kind = "internal"
from = ["Review"]
to = "InProgress"
effect = "increment review_cycles"

[[action]]
name = "ApproveReview"
kind = "internal"
from = ["Review"]
to = "Done"
effect = "increment review_cycles"

[[action]]
name = "ArchiveIssue"
kind = "internal"
from = ["Done"]
to = "Archived"

[[action]]
name = "ReopenIssue"
kind = "input"
from = ["Done"]
to = "InProgress"

[[invariant]]
name = "ArchivedIsFinal"
when = ["Archived"]
assert = "no_further_transitions"
```

### Support Ticket

```toml
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"

[[state]]
name = "replies"
type = "counter"
initial = "0"

[[state]]
name = "customer_responded"
type = "bool"
initial = "false"

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"
params = ["AgentId"]
hint = "Assign a support agent to the ticket."

[[action]]
name = "ReplyToCustomer"
kind = "internal"
from = ["InProgress"]
to = "WaitingOnCustomer"
effect = "increment replies"
hint = "Agent sends a reply and waits for customer response."

[[action]]
name = "CustomerReply"
kind = "input"
from = ["WaitingOnCustomer"]
to = "InProgress"
effect = "set customer_responded true"

[[action]]
name = "ResolveTicket"
kind = "internal"
from = ["InProgress"]
to = "Resolved"
guard = "replies > 0"
hint = "Resolve the ticket. Requires at least one reply."

[[action]]
name = "CloseTicket"
kind = "internal"
from = ["Resolved"]
to = "Closed"

[[action]]
name = "ReopenTicket"
kind = "input"
from = ["Resolved"]
to = "Open"

[[invariant]]
name = "ClosedIsFinal"
when = ["Closed"]
assert = "no_further_transitions"

[[invariant]]
name = "ResolvedNeedsReply"
when = ["Resolved", "Closed"]
assert = "replies > 0"

[[liveness]]
name = "TicketEventuallyResolved"
from = ["Open"]
reaches = ["Resolved", "Closed"]
```

### Payment Flow

```toml
[automaton]
name = "Payment"
states = ["Pending", "Authorized", "Captured", "Failed", "Refunded", "PartiallyRefunded"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
kind = "internal"
from = ["Pending"]
to = "Authorized"

[[action]]
name = "CapturePayment"
kind = "internal"
from = ["Authorized"]
to = "Captured"

[[action]]
name = "FailPayment"
kind = "internal"
from = ["Pending", "Authorized"]
to = "Failed"
params = ["Reason"]

[[action]]
name = "RefundPayment"
kind = "internal"
from = ["Captured"]
to = "Refunded"
params = ["Amount"]

[[action]]
name = "PartialRefund"
kind = "internal"
from = ["Captured"]
to = "PartiallyRefunded"
params = ["Amount"]

[[action]]
name = "PaymentAuthorizedEvent"
kind = "output"

[[action]]
name = "PaymentCapturedEvent"
kind = "output"

[[invariant]]
name = "FailedIsFinal"
when = ["Failed"]
assert = "no_further_transitions"

[[invariant]]
name = "RefundedIsFinal"
when = ["Refunded"]
assert = "no_further_transitions"
```

### Subscription Management

```toml
[automaton]
name = "Subscription"
states = ["Active", "PastDue", "Suspended", "Cancelled", "Expired"]
initial = "Active"

[[state]]
name = "payment_failures"
type = "counter"
initial = "0"

[[state]]
name = "auto_renew"
type = "bool"
initial = "false"

[[action]]
name = "PaymentFailed"
kind = "internal"
from = ["Active"]
to = "PastDue"
effect = "increment payment_failures"

[[action]]
name = "RetryPayment"
kind = "input"
from = ["PastDue"]
to = "Active"

[[action]]
name = "SuspendSubscription"
kind = "internal"
from = ["PastDue"]
to = "Suspended"

[[action]]
name = "ReactivateSubscription"
kind = "input"
from = ["Suspended"]
to = "Active"

[[action]]
name = "CancelSubscription"
kind = "input"
from = ["Active", "PastDue", "Suspended"]
to = "Cancelled"
params = ["Reason"]

[[action]]
name = "ExpireSubscription"
kind = "internal"
from = ["Suspended"]
to = "Expired"

[[action]]
name = "EnableAutoRenew"
kind = "input"
from = ["Active"]
to = "Active"
effect = "set auto_renew true"

[[action]]
name = "DisableAutoRenew"
kind = "input"
from = ["Active"]
to = "Active"
effect = "set auto_renew false"

[[invariant]]
name = "CancelledIsFinal"
when = ["Cancelled"]
assert = "no_further_transitions"

[[invariant]]
name = "ExpiredIsFinal"
when = ["Expired"]
assert = "no_further_transitions"

[[invariant]]
name = "PastDueHasFailure"
when = ["PastDue", "Suspended", "Expired"]
assert = "payment_failures > 0"

[[integration]]
name = "billing_webhook"
trigger = "PaymentFailed"
type = "webhook"

[[integration]]
name = "dunning_notice"
trigger = "SuspendSubscription"
type = "webhook"
```

---

## Error Translation Table

When verification fails, translate the technical output to developer-friendly language. Never show raw verification output unless the developer asks for details.

| Verification Result | Developer-Friendly Explanation | Suggestion |
|---|---|---|
| Dead guard (L0 SMT: guard unsatisfiable) | "The action '[name]' can never fire -- its guard condition is always false given the counter bounds" | Check if the guard references a counter that can never reach the required value, or if the condition contradicts the state variables |
| Non-inductive invariant (L0 SMT) | "The rule '[name]' can be broken by a specific sequence of actions -- a transition reaches the trigger state without establishing the required condition" | The invariant holds initially but some transition violates it. Add a guard to the offending transition, or adjust the effect to maintain the invariant |
| Unreachable state (L0 SMT) | "No sequence of actions leads to '[state]' from the initial state" | Check if you are missing a transition that leads to this state, or remove the state if it is truly unused |
| L1 Model Check FAILED with counterexample | "Here is a specific scenario that breaks the rule: [trace]. The model checker exhaustively explored all possible states and found this violation" | Follow the trace step by step -- the last transition is where things go wrong. Add a guard or fix the invariant |
| L2 Simulation FAILED | "Under fault injection (message delays, drops, crashes), the system violated an invariant after [N] transitions" | The spec is not resilient to concurrent access patterns. Tighten guards or add ordering constraints |
| L3 Property Tests FAILED: invariant '[name]' violated after N actions | "Random testing found a sequence of [N] actions that violates '[name]'" | The failing sequence is the counterexample. Add a guard to prevent the violating action, or fix the invariant assertion |
| Deadlock detected (L1: no actions enabled) | "The entity gets stuck in state '[state]' with no way out and no valid actions" | Add a transition out of the deadlocked state, or mark it as a terminal state with `no_further_transitions` |

### How to Present Results

**Default (non-technical developer):** Show only pass/fail with plain-language explanation of any failure.

```
Verified: Order, Payment, Shipment -- all checks passed.
```

or:

```
Issue with Order spec: An order can reach Submitted with zero items.
Should submission require at least one item? (I can add that guard.)
```

**On request ("show me verification details"):** Show per-level results.

```
Order verification cascade:
  [PASS] L0 Symbolic: 12 guards satisfiable, 4 invariants inductive, 0 unreachable
  [PASS] L1 Model Check: 42,847 states explored, all properties hold
  [PASS] L2 Simulation: 10 seeds, 847 transitions, 12 dropped msgs
  [PASS] L3 Property Tests: 1000 cases, 30 max steps
```

---

## Progressive Disclosure

Start simple, add complexity only when needed.

### Level 1: Minimum Viable Spec (start here)
- 3-5 states, 2-3 actions
- No guards, no effects, no invariants, no state variables
- Just state machine structure
- Verify: should pass trivially

### Level 2: Add State Variables and Guards
- Add `[[state]]` variables (counters, booleans)
- Add guard conditions to transitions
- Run verification to check guard satisfiability (L0)

### Level 3: Add Effects
- Add side effects to transitions: `increment`, `decrement`, `set`
- Effects modify state variables during transitions

### Level 4: Add Invariants
- Add always-true properties with `[[invariant]]`
- Run full verification cascade (L0-L3)
- This is where most bugs are caught

### Level 5: Add Liveness and Integrations
- Add `[[liveness]]` properties (something eventually happens)
- Add `[[integration]]` declarations for external webhooks
- Add `[[action]]` entries with `kind = "output"` for emitted events

### Level 6: Multi-Entity
- Add related entities (Order + Payment + Shipment)
- Each entity is its own spec file and actor
- Define cross-entity references through params (but no cross-entity state coordination in a single spec)

---

## CLI Reference

### Creating a new project

```bash
temper init my-app
```

Creates the project scaffold:
```
my-app/
  specs/
    model.csdl.xml          # OData CSDL data model
    policies/               # Cedar authorization policies
  generated/                # Generated Rust code (do not edit)
  evolution/                # Evolution records
    observations/
    problems/
    analyses/
    decisions/
    insights/
  src/
    main.rs
  Cargo.toml
```

### Verifying specs

```bash
temper verify --specs-dir specs/
```

Runs the full 4-level verification cascade on all `.ioa.toml` files in the directory. Requires `model.csdl.xml` to exist. Output shows per-entity, per-level pass/fail.

The cascade levels:
- **L0 Symbolic (Z3 SMT):** Guard satisfiability, invariant induction, unreachable states
- **L1 Model Check (Stateright):** Exhaustive state-space exploration, safety + liveness
- **L2 Simulation (DST):** Multi-actor fault injection (10 seeds, 3 actors, 200 ticks)
- **L3 Property Tests (proptest):** 1000 random action sequences, invariant checking after each step

All 4 levels must pass before deployment.

### Generating code

```bash
temper codegen --specs-dir specs/ --output-dir generated/
```

Generates Rust entity actors from verified specs. Produces per-entity modules with state structs, status enums, message enums, and transition tables.

Never hand-edit files in `generated/`. They are overwritten on next codegen run.

### Starting the server

```bash
# Verify first, then serve
temper verify --specs-dir specs/

# Start with Postgres persistence
DATABASE_URL=postgres://myapp:myapp_dev@localhost:5432/myapp \
  temper serve --specs-dir specs/ --tenant my-app --port 3000

# Start without persistence (in-memory only, events lost on restart)
temper serve --specs-dir specs/ --tenant my-app
```

`temper serve --specs-dir` runs the verification cascade at startup and rejects invalid specs. The server will not serve unverified entities.

### Testing with curl

The OData API is served at `/tdata` (not `/odata`).

```bash
# Service document (lists entity sets)
curl http://localhost:3000/tdata

# Full metadata (CSDL XML)
curl http://localhost:3000/tdata/\$metadata

# List entities
curl http://localhost:3000/tdata/Orders

# Create entity (spawns actor in initial state)
curl -X POST http://localhost:3000/tdata/Orders \
  -H "Content-Type: application/json" \
  -d '{"title": "My Order"}'

# Invoke a bound action (namespace-qualified)
curl -X POST http://localhost:3000/tdata/Orders\('entity-id'\)/Ns.AddItem \
  -H "Content-Type: application/json" \
  -d '{"ProductId": "p1", "Quantity": 1}'

# Get single entity
curl http://localhost:3000/tdata/Orders\('entity-id'\)

# Query with OData filters
curl "http://localhost:3000/tdata/Orders?\$filter=Status eq 'Draft'&\$top=10"
```

Response format:
```json
{
    "@odata.context": "$metadata#Orders/$entity",
    "entity_type": "Order",
    "entity_id": "019c3949-8405-...",
    "status": "Draft",
    "item_count": 0,
    "fields": {"Id": "019c3949-8405-...", "Status": "Draft"},
    "events": []
}
```

Invalid actions return 409 Conflict:
```json
{
    "error": {
        "code": "ActionFailed",
        "message": "Action 'SubmitOrder' not valid from state 'Draft'"
    }
}
```

### Multi-tenant dispatch

Use the `X-Tenant-Id` header:
```bash
curl -H "X-Tenant-Id: my-app" http://localhost:3000/tdata/Orders
```

---

## CSDL Data Model

Each entity type needs a corresponding entry in `specs/model.csdl.xml`. The CSDL defines the OData API surface (entity types, properties, navigation, actions, functions).

### Entity Type Template

```xml
<EntityType Name="Order">
  <Key><PropertyRef Name="Id"/></Key>
  <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
  <Property Name="Status" Type="Temper.MyApp.OrderStatus" Nullable="false"/>
  <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>

  <Annotation Term="Temper.Vocab.StateMachine.States">
    <Collection>
      <String>Draft</String>
      <String>Submitted</String>
      <String>Cancelled</String>
    </Collection>
  </Annotation>
  <Annotation Term="Temper.Vocab.StateMachine.InitialState" String="Draft"/>
  <Annotation Term="Temper.Vocab.StateMachine.Spec" String="order.ioa.toml"/>
</EntityType>
```

### Bound Action Template

```xml
<Action Name="SubmitOrder" IsBound="true">
  <Parameter Name="bindingParameter" Type="Temper.MyApp.Order"/>
  <Parameter Name="ShippingAddressId" Type="Edm.Guid" Nullable="false"/>
  <ReturnType Type="Temper.MyApp.Order"/>

  <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
    <Collection><String>Draft</String></Collection>
  </Annotation>
  <Annotation Term="Temper.Vocab.StateMachine.TargetState" String="Submitted"/>
  <Annotation Term="Temper.Vocab.Agent.Hint"
    String="Submit a draft order. Requires at least one item."/>
</Action>
```

### IOA ↔ CSDL Mapping Rules

**Every IOA spec file MUST have a matching CSDL EntityType, and vice versa.** Mismatches cause verification failures or runtime dispatch errors. Follow these rules exactly:

#### Entity-Level Mapping

| IOA Field | CSDL Annotation | Rule |
|---|---|---|
| `[automaton].name` | `<EntityType Name="...">` | Must be identical |
| `[automaton].states` | `Temper.Vocab.StateMachine.States` | Every IOA state must appear in CSDL Collection, same order |
| `[automaton].initial` | `Temper.Vocab.StateMachine.InitialState` | Must match exactly |
| (filename) | `Temper.Vocab.StateMachine.Spec` | Must point to the `.ioa.toml` file (e.g., `"task.ioa.toml"`) |

#### Action-Level Mapping

| IOA Field | CSDL Element/Annotation | Rule |
|---|---|---|
| `[[action]].name` | `<Action Name="...">` | Must be identical |
| `[[action]].from` | `Temper.Vocab.StateMachine.ValidFromStates` | Every state in `from` array must appear in CSDL Collection |
| `[[action]].to` | `Temper.Vocab.StateMachine.TargetState` | Must match exactly (omit both if self-loop) |
| `[[action]].params` | `<Parameter Name="..." Type="..."/>` | Each IOA param becomes a CSDL Parameter (after `bindingParameter`) |
| `[[action]].hint` | `Temper.Vocab.Agent.Hint` | Copy hint text verbatim |

#### Rules

1. **Only `kind = "input"` and `kind = "internal"` actions get CSDL `<Action>` entries.** Output actions (`kind = "output"`) are events and do NOT appear as CSDL actions.
2. **Self-loop actions** (no `to` field, or `to` equals a `from` state) still need CSDL actions with `ValidFromStates` but no `TargetState`.
3. **Every CSDL `<Action>` must have `IsBound="true"`** with the first parameter being `bindingParameter` of the entity type.
4. **CSDL namespace** must be consistent (e.g., `Temper.MyApp`) across all EntityTypes and Actions.
5. **Property types**: Map IOA `[[state]]` variables to CSDL Properties — `type = "counter"` → `Edm.Int32`, `type = "bool"` → `Edm.Boolean`.
6. **The `Spec` annotation** must use `Temper.Vocab.StateMachine.Spec` and point to the IOA file (not TLA+).

### Complete Worked Example: Task Entity

This shows **both files together** for a simple Task entity with Draft → InProgress → Done lifecycle.

#### `specs/task.ioa.toml`

```toml
# Task entity with draft/progress/done lifecycle
[automaton]
name = "Task"
states = ["Draft", "InProgress", "Done", "Cancelled"]
initial = "Draft"

[[state]]
name = "edits"
type = "counter"
initial = "0"

[[action]]
name = "StartWork"
kind = "input"
from = ["Draft"]
to = "InProgress"
hint = "Begin working on the task."

[[action]]
name = "EditTask"
kind = "input"
from = ["Draft", "InProgress"]
effect = "increment edits"
hint = "Edit task details. Does not change state."

[[action]]
name = "CompleteTask"
kind = "internal"
from = ["InProgress"]
to = "Done"
hint = "Mark the task as completed."

[[action]]
name = "CancelTask"
kind = "input"
from = ["Draft", "InProgress"]
to = "Cancelled"
params = ["Reason"]
hint = "Cancel the task with a reason."

[[action]]
name = "ReopenTask"
kind = "input"
from = ["Done"]
to = "InProgress"
hint = "Reopen a completed task."

[[invariant]]
name = "CancelledIsFinal"
when = ["Cancelled"]
assert = "no_further_transitions"

[[liveness]]
name = "EventuallyCompleted"
from = ["Draft"]
reaches = ["Done", "Cancelled"]
```

#### `specs/model.csdl.xml` (matching CSDL)

```xml
<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>

    <!-- Temper vocabulary terms -->
    <Schema Namespace="Temper.Vocab" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <Term Name="StateMachine.States" Type="Collection(Edm.String)" AppliesTo="EntityType"/>
      <Term Name="StateMachine.InitialState" Type="Edm.String" AppliesTo="EntityType"/>
      <Term Name="StateMachine.Spec" Type="Edm.String" AppliesTo="EntityType"/>
      <Term Name="StateMachine.ValidFromStates" Type="Collection(Edm.String)" AppliesTo="Action"/>
      <Term Name="StateMachine.TargetState" Type="Edm.String" AppliesTo="Action"/>
      <Term Name="Agent.Hint" Type="Edm.String" AppliesTo="Action EntityType"/>
    </Schema>

    <!-- Application schema -->
    <Schema Namespace="Temper.TaskApp" xmlns="http://docs.oasis-open.org/odata/ns/edm">

      <!-- Entity Type: matches task.ioa.toml exactly -->
      <EntityType Name="Task">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="Edits" Type="Edm.Int32" Nullable="false" DefaultValue="0"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>

        <!-- States must match [automaton].states exactly -->
        <Annotation Term="Temper.Vocab.StateMachine.States">
          <Collection>
            <String>Draft</String>
            <String>InProgress</String>
            <String>Done</String>
            <String>Cancelled</String>
          </Collection>
        </Annotation>
        <!-- Initial must match [automaton].initial -->
        <Annotation Term="Temper.Vocab.StateMachine.InitialState" String="Draft"/>
        <!-- Points to the IOA spec file -->
        <Annotation Term="Temper.Vocab.StateMachine.Spec" String="task.ioa.toml"/>
      </EntityType>

      <!-- Action: StartWork — from=["Draft"] to="InProgress" -->
      <Action Name="StartWork" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.TaskApp.Task"/>
        <ReturnType Type="Temper.TaskApp.Task"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection><String>Draft</String></Collection>
        </Annotation>
        <Annotation Term="Temper.Vocab.StateMachine.TargetState" String="InProgress"/>
        <Annotation Term="Temper.Vocab.Agent.Hint" String="Begin working on the task."/>
      </Action>

      <!-- Action: EditTask — self-loop from=["Draft","InProgress"], no TargetState -->
      <Action Name="EditTask" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.TaskApp.Task"/>
        <ReturnType Type="Temper.TaskApp.Task"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection>
            <String>Draft</String>
            <String>InProgress</String>
          </Collection>
        </Annotation>
        <!-- No TargetState — this is a self-loop (effect only, no state change) -->
        <Annotation Term="Temper.Vocab.Agent.Hint" String="Edit task details. Does not change state."/>
      </Action>

      <!-- Action: CompleteTask — from=["InProgress"] to="Done" -->
      <Action Name="CompleteTask" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.TaskApp.Task"/>
        <ReturnType Type="Temper.TaskApp.Task"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection><String>InProgress</String></Collection>
        </Annotation>
        <Annotation Term="Temper.Vocab.StateMachine.TargetState" String="Done"/>
        <Annotation Term="Temper.Vocab.Agent.Hint" String="Mark the task as completed."/>
      </Action>

      <!-- Action: CancelTask — from=["Draft","InProgress"] to="Cancelled", has params -->
      <Action Name="CancelTask" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.TaskApp.Task"/>
        <Parameter Name="Reason" Type="Edm.String" Nullable="false"/>
        <ReturnType Type="Temper.TaskApp.Task"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection>
            <String>Draft</String>
            <String>InProgress</String>
          </Collection>
        </Annotation>
        <Annotation Term="Temper.Vocab.StateMachine.TargetState" String="Cancelled"/>
        <Annotation Term="Temper.Vocab.Agent.Hint" String="Cancel the task with a reason."/>
      </Action>

      <!-- Action: ReopenTask — from=["Done"] to="InProgress" -->
      <Action Name="ReopenTask" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.TaskApp.Task"/>
        <ReturnType Type="Temper.TaskApp.Task"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection><String>Done</String></Collection>
        </Annotation>
        <Annotation Term="Temper.Vocab.StateMachine.TargetState" String="InProgress"/>
        <Annotation Term="Temper.Vocab.Agent.Hint" String="Reopen a completed task."/>
      </Action>

      <!-- Entity Container -->
      <EntityContainer Name="Default">
        <EntitySet Name="Tasks" EntityType="Temper.TaskApp.Task"/>
      </EntityContainer>

    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
```

**Checklist**: Before generating, verify:
- [ ] Every state in `[automaton].states` appears in `StateMachine.States` Collection
- [ ] `[automaton].initial` matches `StateMachine.InitialState`
- [ ] `Spec` annotation points to the `.ioa.toml` filename
- [ ] Every `kind = "input"` or `kind = "internal"` action has a matching `<Action>`
- [ ] Every action's `from` array matches its `ValidFromStates` Collection
- [ ] Every action's `to` matches its `TargetState` (omit for self-loops)
- [ ] Every action's `params` match `<Parameter>` elements (after `bindingParameter`)
- [ ] `kind = "output"` actions are NOT in CSDL
- [ ] All `<Action>` entries have `IsBound="true"` with correct `bindingParameter` type
- [ ] EntityContainer has an `<EntitySet>` for each EntityType

---

## Cedar Authorization

Cedar policies define who can do what. Place them in `specs/policies/`.

```cedar
// Customers can read their own orders
permit(
    principal is Customer,
    action == Action::"read",
    resource is Order
) when {
    resource.customerId == principal.id
};

// Nobody can modify cancelled orders
forbid(
    principal,
    action in [Action::"update", Action::"submitOrder"],
    resource is Order
) when {
    resource.status == "Cancelled"
};
```

---

## Workflow Summary

```
1. User: "Build me a [thing]"
2. Interview: Extract entities, states, actions, guards, effects, invariants
3. Generate: Create .ioa.toml specs + model.csdl.xml + Cedar policies
4. Verify: Run `temper verify --specs-dir specs/`
   - Fix any failures (translate errors to domain language)
   - Iterate until all 4 levels pass
5. Deploy: Run `temper serve --specs-dir specs/ --tenant my-app`
6. Test: Use curl against the /tdata OData endpoint
7. Iterate: Add complexity gradually
   (state variables -> guards -> effects -> invariants -> liveness -> multi-entity)
```

---

## Anti-Patterns

| Anti-Pattern | Why It Is Wrong | Do This Instead |
|---|---|---|
| Hand-editing files in `generated/` | Will be overwritten on next codegen | Modify the specs, then regenerate |
| Using `[automaton.signature]` or `[[automaton.transition]]` | Not the actual TOML format | Use flat `[automaton]`, `[[action]]`, `[[invariant]]` |
| Guard as structured object | Parser expects a string | Use `guard = "items > 0"` or `guard = "is_true has_reviewer"` |
| Effect as structured object | Parser expects a string | Use `effect = "increment counter"` or `effect = "set flag true"` |
| Skipping verification before serve | Server rejects invalid specs at startup anyway, but errors are harder to diagnose | Always run `temper verify` first |
| Calling actions without checking entity status | Will get 409 Conflict | GET entity first, check `status` field |
| Deploying without DATABASE_URL | Events are lost on restart (silent data loss) | Always set DATABASE_URL for any real deployment |
| Putting webhook logic in guards or effects | Breaks deterministic verification | Use `[[integration]]` declarations for external calls |
| Using `/odata` as the API path | The actual endpoint is `/tdata` | Always use `/tdata` |
| CSDL states don't match IOA states | Runtime dispatch will fail or actions silently rejected | Use the Mapping Rules checklist above — every IOA state and action must appear in CSDL |
| Missing `Spec` annotation or pointing to `.tla` | Server won't find the IOA spec at runtime | Use `Temper.Vocab.StateMachine.Spec` pointing to `entity.ioa.toml` |
