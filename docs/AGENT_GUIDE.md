# Temper Agent Developer Guide

This document is the primary reference for LLM agents building applications with the Temper framework. It covers the full lifecycle: defining specifications, generating code, verifying correctness, running the server, and evolving the system from production feedback.

---

## Table of Contents

1. [Core Concepts](#1-core-concepts)
2. [Project Structure](#2-project-structure)
3. [Specification Layer (What You Edit)](#3-specification-layer)
4. [Code Generation (What You Don't Edit)](#4-code-generation)
5. [Verification Cascade (How You Prove Correctness)](#5-verification-cascade)
6. [Running the Server](#6-running-the-server)
7. [Authorization (Cedar ABAC)](#7-authorization)
8. [Observability — Telemetry as Views](#8-observability--telemetry-as-views)
9. [Evolution Engine (How the System Improves)](#9-evolution-engine)
10. [Trajectory Intelligence (How You Optimize Agents)](#10-trajectory-intelligence)
11. [JIT Optimization (Hot-Swap Without Redeploy)](#11-jit-optimization)
12. [API Reference Quick Guide](#12-api-reference)
13. [Common Workflows](#13-common-workflows)
14. [Anti-Patterns to Avoid](#14-anti-patterns)

---

## 1. Core Concepts

Temper is a **conversational application platform**. The key principle:

> **You describe what you want. The system builds it, verifies it, and evolves it.**

Temper operates through two separated conversational contexts:

| Context | Who | Purpose |
|---------|-----|---------|
| **Developer Chat** | Developer / builder | Interview → generate specs → verify → deploy |
| **Production Chat** | End users | Operate the app → unmet intents → evolution pipeline |

The developer never writes specs by hand. They describe their domain through conversation.
The system generates I/O Automaton specs, CSDL data models, and Cedar policies, runs the
verification cascade, and deploys entity actors — all within the chat.

When end users hit capabilities that don't exist, their unmet intents flow through the
Evolution Engine. The developer reviews and approves changes via the Developer Chat.

### The Lifecycle

```
1. CONVERSE → Developer describes domain in Developer Chat
2. GENERATE → System produces IOA specs + CSDL + Cedar from conversation
3. VERIFY   → 3-level cascade (model check + simulation + property tests)
4. DEPLOY   → Hot-swap: entity actors live, OData API serving
5. USE      → End users operate the app via Production Chat
6. OBSERVE  → Trajectory intelligence captures unmet intents
7. EVOLVE   → Evolution Engine proposes spec changes → developer approves
8. REPEAT   → Back to step 1, system grows from conversation
```

Three types of specification files are generated (not hand-written):

| File | Format | Purpose |
|------|--------|---------|
| `model.csdl.xml` | OData CSDL (XML) | Data model: entity types, properties, navigation, actions, functions |
| `*.ioa.toml` | I/O Automaton TOML | Behavior: state machines, transitions, guards, invariants |
| `*.cedar` | Cedar | Security: attribute-based access control policies |

Everything else is derived from these specs. If behavior needs to change, it happens
through conversation — the system regenerates the specs and re-verifies.

---

## 2. Project Structure

A Temper project has this layout:

```
my-project/
├── specs/                          # SOURCE OF TRUTH — you edit these
│   ├── model.csdl.xml              # OData CSDL data model
│   ├── order.ioa.toml              # I/O Automaton spec for Order entity
│   ├── payment.ioa.toml            # I/O Automaton spec for Payment entity
│   └── policies/
│       ├── order.cedar             # Cedar policies for Order
│       └── customer.cedar          # Cedar policies for Customer
├── generated/                      # GENERATED — do not hand-edit
│   ├── order.rs                    # Generated Order actor
│   ├── customer.rs                 # Generated Customer actor
│   └── mod.rs                      # Module re-exports
├── evolution/                      # EVOLUTION RECORDS — system memory
│   ├── observations/               # O-Records (from sentinels)
│   ├── problems/                   # P-Records (Lamport-style)
│   ├── analyses/                   # A-Records (solutions)
│   ├── decisions/                  # D-Records (human approvals)
│   └── insights/                   # I-Records (product intelligence)
├── src/
│   └── main.rs                     # Application entry point
└── Cargo.toml
```

Create a new project:
```bash
temper init my-project
```

---

## 3. Specification Layer

### 3.1 CSDL (Data Model)

CSDL defines **what** your API exposes. It follows the OData v4 Common Schema Definition Language.

#### Entity Types

```xml
<EntityType Name="Order">
  <Key><PropertyRef Name="Id"/></Key>
  <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
  <Property Name="Status" Type="Temper.MyApp.OrderStatus" Nullable="false"/>
  <Property Name="CustomerId" Type="Edm.Guid" Nullable="false"/>
  <Property Name="Total" Type="Edm.Decimal" Precision="19" Scale="4"/>
  <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>

  <!-- Navigation properties (relationships) -->
  <NavigationProperty Name="Customer" Type="Temper.MyApp.Customer" Nullable="false">
    <ReferentialConstraint Property="CustomerId" ReferencedProperty="Id"/>
  </NavigationProperty>
  <NavigationProperty Name="Items" Type="Collection(Temper.MyApp.OrderItem)"
                      ContainsTarget="true"/>

  <!-- State machine annotations (link to automaton spec) -->
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

#### Actions (Side-Effecting Operations)

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
    String="Submit a draft order. Requires at least one item and a valid shipping address."/>
</Action>
```

#### Functions (Read-Only Operations)

```xml
<Function Name="GetOrderTotal" IsBound="true">
  <Parameter Name="bindingParameter" Type="Temper.MyApp.Order"/>
  <ReturnType Type="Edm.Decimal" Precision="19" Scale="4"/>
</Function>
```

#### Temper Custom Annotations

| Annotation | Applies To | Purpose |
|-----------|-----------|---------|
| `StateMachine.States` | EntityType | Valid states for this entity |
| `StateMachine.InitialState` | EntityType | Starting state |
| `StateMachine.Spec` | EntityType | Path to I/O Automaton specification file |
| `StateMachine.ValidFromStates` | Action | States this action can fire from |
| `StateMachine.TargetState` | Action | State after action completes |
| `Agent.Hint` | Action, Function, EntityType | Usage hint for LLM agents |
| `Agent.CommonPattern` | Action, Function | Typical successful trajectory pattern |
| `Agent.SuccessRate` | Action | Historical success rate (from trajectories) |
| `ShardKey` | EntityType | Property used for sharding |
| `AuthZ.CedarPolicy` | EntityType, Action | Path to Cedar policy file |

### 3.2 I/O Automaton TOML (Behavioral Specification)

I/O Automaton TOML defines **how** entities behave — state machines, transition guards, and invariants.  Based on the Lynch-Tuttle I/O Automata formalism, each action has a precondition (from + guard) and an effect (to + state changes).

```toml
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Cancelled"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]
to = "Submitted"
guard = "items > 0"
params = ["ShippingAddressId", "PaymentMethod"]
hint = "Submit a draft order. Requires at least one item."

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted"]
to = "Cancelled"
params = ["Reason"]
hint = "Cancel an order. Only from Draft or Submitted."

[[invariant]]
name = "SubmitRequiresItems"
when = ["Submitted"]
assert = "items > 0"
```

**Rules for I/O Automaton specs:**

1. `[automaton]`: Define `name`, `states` (all valid status values), and `initial` state.
2. `[[state]]`: Declare state variables with `name`, `type` (`counter`, `bool`, `string`, `set`), and `initial` value.
3. `[[action]]`: Define actions with `name`, `kind` (`input`/`output`/`internal`), `from` states, `to` state, optional `guard`, `params`, and `hint`.
4. `[[invariant]]`: Define safety invariants with `name`, `when` (trigger states), and `assert` expression.
5. Action kinds: `input` = from environment (HTTP), always enabled in from-states; `output` = emitted events; `internal` = private state transitions.

### 3.3 Cedar (Access Control)

Cedar policies define **who** can do **what** to **which** resources.

```cedar
// Customers can read their own orders
permit(
    principal is Customer,
    action == Action::"read",
    resource is Order
) when {
    resource.customerId == principal.id
};

// Agents inherit customer permissions
permit(
    principal is Agent,
    action in [Action::"read", Action::"submitOrder"],
    resource is Order
) when {
    principal.role == "customer_agent" &&
    resource.customerId == principal.actingFor
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

## 4. Code Generation

Generate Rust actor code from specs:

```bash
temper codegen --specs-dir specs --output-dir generated
```

This produces for each entity:
- **State struct**: `OrderState { id, status, customer_id, total, ... }`
- **Status enum**: `OrderStatus { Draft, Submitted, Cancelled, ... }`
- **Message enum**: `OrderMsg { SubmitOrder { ... }, CancelOrder { ... }, GetState }`
- **Transition table**: `OrderTransitions::can_transition(state, action) -> bool`
- **Invariant names**: `OrderInvariants::invariant_names()`

**IMPORTANT: Never hand-edit files in `generated/`.** They will be overwritten on next codegen run. If you need to change behavior, modify the specs.

### How Entity Actors Work at Runtime

At runtime, entities are NOT served by the generated Rust code directly. Instead, the server builds a **JIT TransitionTable** from the I/O Automaton specification using `TransitionTable::from_tla_source()`. This table is the same verified artifact that passes the 3-level cascade. Each entity gets its own actor:

```
HTTP Request → OData Parse → Actor Registry (get or spawn) → Entity Actor → TransitionTable.evaluate() → Response
```

The entity actor holds:
- `status`: Current state machine state (e.g., "Draft", "Submitted")
- `item_count`: Number of items (for guards like `SubmitRequiresItems`)
- `fields`: All entity fields as JSON
- `events`: Append-only event log of all transitions

When an action is dispatched, the actor evaluates it through the TransitionTable:
1. Find matching rule by action name
2. Check `from_states` guard (is current status valid for this action?)
3. Check additional guards (e.g., `ItemCountMin(1)` for SubmitOrder)
4. If guards pass: apply effects (SetState, IncrementItems, EmitEvent), record event
5. If guards fail: return 409 Conflict with error message

**Critical**: Always use `TransitionTable::from_tla_source(spec_source)` in production, NOT `from_state_machine(sm)`. The `from_tla_source` variant resolves guard predicates from the specification source, producing correct guard constraints. The `from_state_machine` variant may miss guards that reference predicates.

---

## 5. Verification Cascade

Run the three-level verification cascade:

```bash
temper verify --specs-dir specs
```

### Level 1: Stateright Model Checking
- **What**: Exhaustively explores every reachable state of the state machine
- **Finds**: Invariant violations, deadlocks, unreachable states
- **Guarantee**: If it passes, the invariant holds in ALL possible states

### Level 2: Deterministic Simulation
- **What**: Runs multi-actor scenarios with fault injection (message delay, drop, actor crash)
- **Seed-based**: Same seed = identical execution. Failures are reproducible.
- **Faults**: Light (10% delay), Heavy (30% delay, 5% drop, 2% crash)
- **Runs**: 10 seeds by default, each with 3 actors and 200 ticks

### Level 3: Property-Based Tests
- **What**: Generates random action sequences, checks invariants after each step
- **Shrinking**: When a failure is found, proptest finds the minimal counterexample
- **Cases**: 1000 random sequences of up to 30 steps each

**All three levels must pass before deployment.**

---

## 6. Running the Server

```bash
temper serve --port 3000
# Or directly:
cargo run -p your-app
```

The server exposes OData v4 endpoints:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/odata` | Service document (lists entity sets) |
| GET | `/odata/$metadata` | CSDL XML (full data model) |
| GET | `/odata/Orders` | List orders (with $filter, $select, $expand, $orderby, $top, $skip) |
| GET | `/odata/Orders('id')` | Get single order |
| POST | `/odata/Orders` | Create order |
| POST | `/odata/Orders('id')/Ns.SubmitOrder` | Invoke bound action |
| GET | `/odata/Orders('id')/Ns.GetOrderTotal()` | Invoke bound function |

### What Responses Look Like

**POST /odata/Orders** (create — spawns actor in Draft):
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

**POST /odata/Orders('id')/Ns.AddItem** (action — real transition):
```json
{
    "@odata.context": "$metadata#Orders/$entity",
    "status": "Draft",
    "item_count": 1,
    "events": [
        {"action": "AddItem", "from_status": "Draft", "to_status": "Draft", "params": {"ProductId": "p1"}}
    ]
}
```

**POST /odata/Orders('id')/Ns.SubmitOrder** (guard enforced):
```json
{
    "status": "Submitted",
    "item_count": 1,
    "events": [
        {"action": "AddItem", "from_status": "Draft", "to_status": "Draft"},
        {"action": "SubmitOrder", "from_status": "Draft", "to_status": "Submitted"}
    ]
}
```

**Invalid action (409 Conflict)** — e.g., SubmitOrder with 0 items or CancelOrder from Shipped:
```json
{
    "error": {
        "code": "ActionFailed",
        "message": "Action 'SubmitOrder' not valid from state 'Draft'"
    }
}
```

The `events` array is the entity's full audit trail — every state transition with timestamps and parameters. Events are persisted to Postgres (when `DATABASE_URL` is set) and emitted as OTEL spans + metrics via Telemetry as Views (when `OTLP_ENDPOINT` is set). The OTEL SDK exports to any OTLP-compatible backend (ClickHouse, Datadog, Grafana, Jaeger).

### OData Query Examples

```
GET /odata/Orders?$filter=Status eq 'Draft' and Total gt 100.0
GET /odata/Orders?$select=Id,Status,Total&$orderby=CreatedAt desc&$top=10
GET /odata/Orders('abc')?$expand=Items($select=ProductName,Quantity)
GET /odata/Customers('xyz')?$expand=Orders($filter=Status ne 'Cancelled')
```

### Headers for Agent Authentication

```
X-Temper-Principal-Id: agent-1
X-Temper-Principal-Kind: agent
X-Temper-Agent-Role: customer_agent
X-Temper-Acting-For: customer-456
X-Temper-Correlation-Id: trace-abc-123
```

For trajectory tracking:
```
X-Temper-Trajectory: trace_id=abc-123,turn=2
X-Temper-Agent: prompt_version=v7,model=claude-sonnet-4-5-20250929
```

---

## 7. Authorization

Every request goes through Cedar policy evaluation:

```
Request → SecurityContext → Cedar Evaluate → Allow/Deny
```

The SecurityContext is built from HTTP headers. Cedar policies are loaded from `specs/policies/`. Policies can reference:
- `principal.id`, `principal.role`, `principal.actingFor`
- `resource.status`, `resource.customerId`, `resource.total`
- `context.rateLimitExceeded`, `context.timeOfDay`

System principals (internal processes) bypass all checks. Denied requests return `403 Forbidden` with the Cedar policy reason. The engine defaults to permissive (allow-all) and is configured with Cedar policy files at startup.

---

## 8. Observability — Telemetry as Views

Temper uses **Telemetry as Views**: agents don't write instrumentation code. Every entity actor transition automatically emits a "wide event" containing all context. The platform projects it into two views:

| View | Contains | Purpose | Retention |
|------|----------|---------|-----------|
| **Aggregated (Metrics)** | Measurements + Tags (low-cardinality) | Monitoring, alerting, SLOs | Long |
| **Contextual (Spans)** | Everything (tags + attributes + measurements) | Debugging, investigation, trajectories | Short |

### How It Works

Every `EntityEvent` is automatically converted to a `WideEvent` with three field types:

- **Tags** (low-cardinality, in both views): `entity_type`, `operation`, `status`, `success`
- **Attributes** (high-cardinality, contextual only): `entity_id`, `params`, `from_status`
- **Measurements** (numeric, aggregated in metrics): `transition_count`, `duration_ms`, `item_count`

The platform then projects via the OTEL SDK:
```
Actor Transition → WideEvent
    ├── emit_metrics() → OTEL Meter → OTLP exporter → any backend
    │    temper.SubmitOrder.duration_ms{entity_type=Order,operation=SubmitOrder}
    └── emit_span()    → OTEL Tracer → OTLP exporter → any backend
         Order.SubmitOrder span with entity_id, params, from_status, measurements
```

When OTEL is not initialized (e.g., tests or local dev without `OTLP_ENDPOINT`), the global no-op tracer/meter silently discards data — no conditional logic needed.

### Cost Decoupling

`entity_id` is an Attribute (NOT a Tag). This means:
- **In metrics**: zero cost — not a metric tag, no cardinality explosion
- **In traces**: full detail — available for debugging
- **At runtime**: an operator can promote it to a Tag if they need metric-level precision for a specific investigation

No code change needed. No agent involvement. The classification is a platform policy.

### SQL Query Interface

All queries use SQL through the `ObservabilityStore` trait (provider-agnostic):

```sql
-- Metrics: precise aggregation
SELECT metric_name, avg(value) FROM metrics
WHERE metric_name = 'temper.SubmitOrder.duration_ms'
GROUP BY toStartOfMinute(timestamp)

-- Spans: full context
SELECT trace_id, attributes FROM spans
WHERE operation = 'Order.SubmitOrder' AND status = 'error'

-- Exemplar: jump from metric to trace
SELECT tags FROM metrics WHERE metric_name = 'temper.SubmitOrder.duration_ms'
-- → tags contains exemplar.trace_id → click to see the full trace
```

Evolution records reference these as portable SQL. Swapping providers doesn't break evidence chains.

---

## 9. Evolution Engine

The Evolution Engine is how the system improves from production feedback. It produces an immutable chain of records:

```
O-Record (Observation) → P-Record (Problem) → A-Record (Analysis) → D-Record (Decision)
```

Plus I-Records (Insights) for product intelligence.

### Record Types

**O-Record (Observation)**: Detected anomaly from production telemetry.
```toml
[observation]
id = "O-2024-0042"
source = "sentinel:latency"
classification = "Performance"
evidence_query = "SELECT p99(duration_ns) FROM spans WHERE operation = 'handle:SubmitOrder'"
threshold_value = 100000000
observed_value = 450000000
```

**P-Record (Problem)**: Lamport-style formal problem statement.
```toml
[problem]
id = "P-2024-0042"
derived_from = "O-2024-0042"
problem_statement = "Order processing p99 latency exceeds SLO under high concurrency..."
invariants = ["Each order's state transitions remain serializable"]
constraints = ["Cannot change the Order state machine"]
```

**A-Record (Analysis)**: Root cause + proposed solutions with spec diffs.
```toml
[analysis]
id = "A-2024-0042"
derived_from = "P-2024-0042"
root_cause = "Shard key causes hotspot under regional bulk operations"
recommendation = 0  # option index

[[options]]
description = "Compound shard key: entity_id + region"
spec_diff = "+ShardKey: entity_id,region"
invariant_impact = "NONE"
risk = "low"
```

**D-Record (Decision)**: Human approval/rejection.
```toml
[decision]
id = "D-2024-0042"
derived_from = "A-2024-0042"
decision = "Approved"
decided_by = "alice@company.com"
rationale = "Low risk, addresses root cause"
```

### Agent Workflow for Evolution

1. **SentinelActor** (built-in) detects anomaly → creates O-Record
2. **External LLM agent** reads O-Record + Logfire data → creates P-Record and A-Record
3. Agent submits as **Git PR** with the record chain + spec diffs
4. **Human reviews** the PR (problem statement + analysis + verification results)
5. Human merges → D-Record created → codegen → verify → deploy

---

## 10. Trajectory Intelligence

When agents use the OData API, their interaction sequences (trajectories) are captured as structured traces.

### What Trajectories Reveal

| Signal | Meaning | Action |
|--------|---------|--------|
| **Unmet Intent** | Agent can't accomplish user's goal (>70% failure) | Build the missing feature |
| **Friction** | Goal achieved but 3x+ more API calls than optimal | Add convenience action or $expand |
| **Workaround** | Agent cobbles together multi-step hack | Add composite action |

### Trajectory-Enriched $metadata

The `$metadata` endpoint is dynamically enriched with learned patterns:

```xml
<Annotation Term="Temper.Vocab.Agent.Hint"
    String="Check Order.status before calling Cancel. Cancel is only valid
            from {Draft, Confirmed}. For shipped orders, use InitiateReturn."/>
<Annotation Term="Temper.Vocab.Agent.CommonPattern"
    String="1. GET Order. 2. Check status. 3. POST Cancel or InitiateReturn."/>
<Annotation Term="Temper.Vocab.Agent.SuccessRate" Float="0.73"/>
```

**Always read `$metadata` before interacting with an entity.** The Agent.Hint annotations contain critical information about valid transitions and common patterns.

### Feedback Endpoint

After completing a trajectory, submit feedback:
```
POST /odata/$feedback
{
    "TraceId": "abc-123",
    "Score": 0.8,
    "Signal": "task_completed",
    "Comment": "worked but took an extra step"
}
```

---

## 11. JIT Optimization

Three tiers of execution, from most to least rigid:

| Tier | What Changes | How | Needs Redeploy? |
|------|-------------|-----|-----------------|
| **Compiled** | Full Rust actor code | codegen → build → deploy | Yes |
| **Interpretable** | Transition tables (data) | hot-swap via SwapController | No |
| **Overlay** | Query plans, cache TTLs, placement | autonomous optimizer actors | No |

### Hot-Swap Protocol

```
1. Agent generates new TransitionTable from modified spec
2. Verification cascade runs on new table
3. Shadow test: compare old and new tables on test cases
4. If shadow test passes: SwapController.swap(new_table)
5. If production degrades: automatic rollback
```

---

## 12. API Reference Quick Guide

### OData Type Mapping

| OData Type | Rust Type | Example |
|-----------|-----------|---------|
| Edm.Guid | Uuid | `550e8400-e29b-41d4-a716-446655440000` |
| Edm.String | String | `"hello"` |
| Edm.Int32 | i32 | `42` |
| Edm.Int64 | i64 | `9999999999` |
| Edm.Boolean | bool | `true` |
| Edm.Decimal | Decimal | `99.99` |
| Edm.DateTimeOffset | DateTime<Utc> | `2024-03-15T14:30:00Z` |
| Collection(T) | Vec<T> | `[1, 2, 3]` |

### $filter Operators

| Operator | Example |
|----------|---------|
| eq | `Status eq 'Draft'` |
| ne | `Status ne 'Cancelled'` |
| gt, ge, lt, le | `Total gt 100.0` |
| and, or | `Status eq 'Draft' and Total gt 50` |
| not | `not Status eq 'Cancelled'` |
| contains | `contains(Name, 'widget')` |
| startswith | `startswith(Name, 'Ord')` |

---

## 13. Common Workflows

### Adding a New Entity Type

1. Add `<EntityType>` to `model.csdl.xml` with properties, key, and navigation
2. Add `<EntitySet>` to the `<EntityContainer>`
3. Write an I/O Automaton spec (`entity.ioa.toml`) with states, actions, invariants
4. Link via `<Annotation Term="Temper.Vocab.StateMachine.Spec" String="entity.ioa.toml"/>`
5. Write Cedar policies in `specs/policies/entity.cedar`
6. Run `temper codegen` then `temper verify`

### Adding a New Action to an Existing Entity

1. Add `<Action>` to CSDL with parameters and `ValidFromStates` annotation
2. Add the action to the automaton spec (from/to states + guard)
3. Update Cedar policies if needed
4. Run `temper codegen` then `temper verify`

### Changing a State Machine

1. Modify the automaton spec (add/remove states, change actions)
2. Update CSDL `StateMachine.States` annotation to match
3. Update any action `ValidFromStates` annotations
4. Run `temper verify` — the cascade will catch any invariant violations
5. Run `temper codegen` to regenerate actors

### Responding to an Evolution Record

1. Read the O-Record (what was observed) and P-Record (what the problem is)
2. Analyze the root cause using the SQL evidence queries
3. Propose spec changes in an A-Record
4. Run `temper verify` on the proposed changes
5. Submit as a Git PR for human review

---

## 14. Anti-Patterns to Avoid

| Anti-Pattern | Why It's Wrong | Do This Instead |
|-------------|---------------|-----------------|
| Hand-editing generated code | Will be overwritten on next codegen | Modify the CSDL/automaton specs |
| Using `from_state_machine()` in production | Misses `CanXxx` guard resolution | Use `TransitionTable::from_tla_source()` |
| Skipping verification | Deploys unverified state machines | Always run `temper verify` |
| Calling actions without checking status | Will get 409 Conflict | GET entity first, check `status` field |
| Calling actions without reading $metadata | May attempt invalid transitions | Read Agent.Hint annotations first |
| Hard-coding entity URLs | Breaks if entity set names change | Read service document at `/odata` first |
| Writing provider-specific observability queries | Breaks when swapping Logfire↔Datadog | Use canonical SQL schema (spans, logs, metrics) |
| Modifying Cedar policies without human approval | Security change requires human gate | Submit as evolution A-Record for review |
| Creating evolution records without evidence | Unverifiable claims | Include SQL evidence queries in O-Records |
| Putting guards inline in action params | Guards are separate from action parameters | Use `guard = "items > 0"` for preconditions, `params = [...]` for action inputs |

---

## 15. TigerStyle Engineering Philosophy

Temper follows [TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md), TigerBeetle's engineering discipline. Key principles applied:

### Assertions Are Not Just for Testing

Every entity actor transition has pre and postcondition assertions that run in production (`debug_assert!`):

```
PRECONDITION:  status must be in valid state set
PRECONDITION:  event budget not exhausted (< 10,000)
PRECONDITION:  item count within budget (<= 1,000)
--- transition executes ---
POSTCONDITION: status must still be in valid state set
POSTCONDITION: event log grew by exactly 1
POSTCONDITION: last event matches the action that fired
```

These are the automaton invariants enforced at runtime. The TransitionTable guards are production assertions — if Stateright proved the invariant holds across all 42,847 states, the assertion will never fire. But if a code change breaks an assumption, it fires immediately rather than corrupting state silently.

### Bounded Execution — Budgets, Not Limits

Everything has a hard budget:
- `MAX_EVENTS_PER_ENTITY = 10,000` — entity refuses transitions after this
- `MAX_ITEMS_PER_ENTITY = 1,000` — item additions rejected past this
- Mailbox depth is bounded (not unbounded queues)
- Simulation ticks are bounded (max 500)
- Property test sequences are bounded (max 30 steps)

When a budget is exceeded, the system fails fast with a clear error — no OOM, no slow degradation, no tail latency spikes.

### Deterministic Simulation Is the Primary Testing Strategy

DST is not an afterthought — it's the first test you write. Before any HTTP wiring, before any integration test, you write a DST test that exercises the actor through the runtime. This caught 3 guard resolution bugs that no other testing strategy would have found.

### Zero Technical Debt

If a spec change passes the 3-level verification cascade, it ships. If it doesn't, the cascade tells you exactly why and what to fix. There's no "we'll fix the invariant violation later" — the cascade is a hard gate.

---

## 16. DST-First Development Methodology

When adding new features or changing state machines, follow the **DST-first** approach:

1. **Write the automaton spec change** (add states, actions, invariants)
2. **Write DST tests** that exercise the new behavior through the actor system:
   ```rust
   #[tokio::test]
   async fn dst_new_feature() {
       let system = ActorSystem::new("dst");
       let table = Arc::new(TransitionTable::from_tla_source(MY_TLA));
       let actor = EntityActor::new("Order", "test-1", table, json!({}));
       let actor_ref = system.spawn(actor, "test-1");
       // Exercise the new transition...
   }
   ```
3. **Run DST tests** — they will fail if guards are wrong, transitions are missing, or invariants are violated
4. **Fix bugs found by DST** — these are real bugs that would manifest in production
5. **Run the full verification cascade** (`temper verify`) to prove correctness at all 3 levels
6. **Wire into HTTP** — the same TransitionTable that passes DST runs in the entity actors

This approach caught three real bugs during Temper's own development:
- `SubmitOrder` succeeding with 0 items (guard not enforced)
- `CancelOrder` missing entirely from the transition table (filtered as a guard predicate)
- Guard predicates not detected for parameterized actions like `CancelOrder(reason)`

All three were found by DST tests before any HTTP request was made.

---

## Appendix A: Infrastructure Setup

```bash
# Start Postgres, Redis, ClickHouse, OTEL Collector
docker compose up -d

# Copy environment template
cp .env.example .env

# Start server with persistence + OTEL telemetry
DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper \
OTLP_ENDPOINT=http://localhost:4318 \
cargo run -p ecommerce

# Run the conversational agent (needs ANTHROPIC_API_KEY)
OTLP_ENDPOINT=http://localhost:4318 cargo run -p ecommerce -- agent

# Run a single agent command
OTLP_ENDPOINT=http://localhost:4318 cargo run -p ecommerce -- agent "Create an order and submit it"

# Run trajectory analysis (reads from ClickHouse)
CLICKHOUSE_URL=http://localhost:8123 cargo run -p ecommerce -- analyze

# Run the full E2E demo
./scripts/demo.sh
```

## Appendix B: CLI Reference

```
temper init <name>              Create a new Temper project
temper codegen [--specs-dir DIR] [--output-dir DIR]
                                Generate Rust code from specs
temper verify [--specs-dir DIR] Run 3-level verification cascade
temper serve [--port PORT]      Start development server (default: 3000)
```

## Appendix C: Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `DATABASE_URL` | For persistence | Postgres connection string |
| `REDIS_URL` | For caching | Redis connection string |
| `OTLP_ENDPOINT` | For telemetry export | OTLP collector base URL (e.g., `http://localhost:4318`) |
| `CLICKHOUSE_URL` | For analysis queries | ClickHouse HTTP endpoint (read path for trajectory analysis) |
| `ANTHROPIC_API_KEY` | For agent mode | Claude API key |
| `RUST_LOG` | No | Log level (default: `info,temper=debug`) |
