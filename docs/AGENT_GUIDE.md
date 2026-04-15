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
9. [Integration Engine (External System Webhooks)](#9-integration-engine)
10. [Evolution Engine (How the System Improves)](#10-evolution-engine)
11. [Trajectory Intelligence (How You Optimize Agents)](#11-trajectory-intelligence)
12. [JIT Optimization (Hot-Swap Without Redeploy)](#12-jit-optimization)
13. [Performance Characteristics](#13-performance-characteristics)
14. [API Reference Quick Guide](#14-api-reference)
15. [Common Workflows](#15-common-workflows)
16. [Anti-Patterns to Avoid](#16-anti-patterns)
17. [Conversational Vision: How to Represent Temper to Users](#17-conversational-vision)

---

## 1. Core Concepts

Temper is a **conversational application platform**. The key principle:

> **You describe what you want. The system builds it, verifies it, and evolves it.**

Temper operates through two separated conversational contexts:

| Context | Who | Purpose |
|---------|-----|---------|
| **Developer Chat** | Developer / builder | Interview → generate specs → verify → deploy |
| **Production Chat** | End users | Operate the app → unmet intents → evolution pipeline |

### Development Flow (All Projects)

All Temper projects follow the same development loop:

```
1. CONVERSE  → Developer + coding agent discuss the domain
2. GENERATE  → Agent produces IOA specs + CSDL + Cedar
3. VERIFY    → System generates DST scenarios, model checks, property tests
4. REVIEW    → Developer reviews cascade results, counterexamples, coverage
5. ITERATE   → Adjust specs until intent + verification are locked in
6. DEPLOY    → Choose self-host or platform-host (see below)
```

The developer never writes specs by hand. They describe their domain through conversation.
The coding agent generates I/O Automaton specs, CSDL data models, and Cedar policies, runs the
verification cascade, and iterates until all levels pass.

When end users hit capabilities that don't exist, their unmet intents flow through the
Evolution Engine. The developer reviews and approves changes via the Developer Chat.

### Deployment Options

| Option | How | What the agent produces | Status |
|--------|-----|------------------------|--------|
| **Self-host** | `temper codegen` → `cargo build` → deploy binary | Specs + Rust crate with DST tests + infrastructure | Production-ready |
| **Platform-host** | `temper serve --specs-dir --tenant` | Specs only | Multi-tenant, future: hot-swap transitions |

**Self-host path:** The coding agent produces a Cargo crate with specs, full verification cascade
(SMT + Stateright + DST + property tests), and infrastructure (Docker Compose). The developer
builds and deploys the binary. See `reference-apps/ecommerce/` for the canonical example.

**Platform-host path:** The developer provides specs to `temper serve --specs-dir`, which runs
the VerificationCascade at startup and rejects invalid specs. Multi-tenant hosting with
domain-specific servers per tenant.

**Single-node architecture.** The current runtime is single-process. Actor mailboxes
use local `tokio::sync::mpsc` channels, not Redis.

This is safe for the current request-response model because the Postgres event
journal is the durable record. If the server crashes:
1. In-flight `mpsc` messages are lost, but the HTTP caller gets a connection error
2. On restart, actors replay their event journal from Postgres to rebuild state
3. The caller retries — the actor is back at the last committed state

Redis-backed mailboxes become necessary for **async inter-actor messaging** in a
distributed deployment (messages between actors on different nodes). The
`temper-store-redis` crate defines `MailboxStore`, `PlacementStore`, and
`CacheStore` traits with in-memory stubs for this future. `REDIS_URL` is not
read by the server today.

### The Full Lifecycle

```
1. CONVERSE → Developer describes domain in Developer Chat
2. GENERATE → System produces IOA specs + CSDL + Cedar from conversation
3. VERIFY   → 4-level cascade (SMT + model check + simulation + property tests)
4. DEPLOY   → Self-host binary or platform-host via temper serve
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

At runtime, entities are NOT served by the generated Rust code directly. Instead, the server builds a **JIT TransitionTable** from the I/O Automaton specification using `TransitionTable::from_ioa_source()`. Both the verification model and the runtime table derive from the same parsed `Automaton`, ensuring the behavior verified by the four-level cascade is identical to what runs in production. Each entity gets its own actor:

```
HTTP Request → OData Parse → Actor Registry (get or spawn) → Entity Actor → TransitionTable.evaluate() → Response
```

The entity actor holds:
- `status`: Current state machine state (e.g., "Draft", "Submitted")
- `counters`: Named counter variables (e.g., `{"items": 2, "spec_count": 1}`)
- `booleans`: Named boolean variables (e.g., `{"has_address": true}`)
- `fields`: All entity fields as JSON
- `events`: Append-only event log of all transitions

When an action is dispatched, the actor evaluates it through the TransitionTable
with a full `EvalContext` containing counters and booleans:
1. Find matching rule by action name
2. Check `from_states` guard (is current status valid for this action?)
3. Check additional guards (`CounterMin`, `BoolTrue`, compound `And`)
4. If guards pass: apply effects (`SetState`, `IncrementCounter`, `SetBool`, `EmitEvent`, `Custom`), record event. `EmitEvent` feeds the Integration Engine for external webhooks (see [Section 9](#9-integration-engine)).
5. If guards fail: return 409 Conflict with error message

**Critical**: `TransitionTable::from_ioa_source(ioa_toml)` is the sole production constructor. The TLA+ code path has been fully removed.

---

## 5. Verification Cascade

Run the four-level verification cascade:

```bash
temper verify --specs-dir specs
```

### Level 0: Z3 SMT Symbolic Verification
- **What**: Encodes guards and invariants as Z3 formulas, checks algebraically
- **Finds**: Dead guards (actions that can never fire), non-inductive invariants, unreachable states
- **Advantage**: Works on unbounded state spaces without enumerating states

### Level 1: Stateright Model Checking
- **What**: Exhaustively explores every reachable state of the multi-variable state machine
- **Checks**: Safety invariants (always), liveness properties (eventually/no-deadlock)
- **State**: Tracks status + named counters (BTreeMap) + named booleans (BTreeMap)
- **Guarantee**: If it passes, the invariant holds in ALL reachable states

### Level 2: Deterministic Simulation
- **What**: Runs multi-actor scenarios with fault injection (message delay, drop, actor crash)
- **Seed-based**: Same seed = identical execution. Failures are reproducible.
- **Faults**: Light (10% delay), Heavy (30% delay, 5% drop, 2% crash)
- **Runs**: 10 seeds by default, each with 3 actors and 200 ticks

### Level 3: Property-Based Tests
- **What**: Generates random action sequences, checks invariants after each step
- **Shrinking**: When a failure is found, proptest finds the minimal counterexample
- **Cases**: 1000 random sequences of up to 30 steps each

**All four levels must pass before deployment.**

---

## 6. Running the Server

### Compile-First Path (Recommended for Coding Agents)

The safest onboarding path: write specs to disk, verify, then serve.

```bash
# 1. Verify specs pass the 4-level cascade
temper verify --specs-dir specs

# 2. Start server with verified specs + Postgres persistence
DATABASE_URL=postgres://myapp:myapp_dev@localhost:5432/myapp \
  temper serve --specs-dir specs --tenant my-app --port 3000
```

`temper serve --specs-dir` runs the verification cascade on every IOA spec before loading them.
Invalid specs are rejected at startup — the server will not serve unverified entities.

**IMPORTANT: Without `DATABASE_URL`, the server runs in-memory only.** Entity state
is lost on restart. The server will log "No DATABASE_URL — running in-memory only"
at startup. Always set `DATABASE_URL` for any deployment beyond local dev exploration.

The server exposes OData v4 endpoints:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/tdata` | Service document (lists entity sets) |
| GET | `/tdata/$metadata` | CSDL XML (full data model) |
| GET | `/tdata/Orders` | List orders (with $filter, $select, $expand, $orderby, $top, $skip) |
| GET | `/tdata/Orders('id')` | Get single order |
| POST | `/tdata/Orders` | Create order |
| POST | `/tdata/Orders('id')/Ns.SubmitOrder` | Invoke bound action |
| GET | `/tdata/Orders('id')/Ns.GetOrderTotal()` | Invoke bound function |

Use the `X-Tenant-Id` header for multi-tenant dispatch. If omitted, the server falls back to the first registered tenant.

### What Responses Look Like

**POST /tdata/Orders** (create — spawns actor in Draft):
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

**POST /tdata/Orders('id')/Ns.AddItem** (action — real transition):
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

**POST /tdata/Orders('id')/Ns.SubmitOrder** (guard enforced):
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
GET /tdata/Orders?$filter=Status eq 'Draft' and Total gt 100.0
GET /tdata/Orders?$select=Id,Status,Total&$orderby=CreatedAt desc&$top=10
GET /tdata/Orders('abc')?$expand=Items($select=ProductName,Quantity)
GET /tdata/Customers('xyz')?$expand=Orders($filter=Status ne 'Cancelled')
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

## 9. Integration Engine

Integrations follow the **Outbox Pattern**: the state machine stays pure and deterministically verifiable; external calls happen out-of-band. `[[integration]]` declarations in IOA TOML are metadata — they don't affect state transitions or verification.

### Spec Syntax

Declare integrations alongside your automaton:

```toml
[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"
```

The `trigger` names an action. When that action fires, the integration engine picks it up asynchronously.

### Runtime Architecture

```
Entity Actor transition
  → Effect::EmitEvent("SubmitOrder")
  → mpsc channel
  → IntegrationEngine (background tokio task)
  → IntegrationRegistry.lookup("SubmitOrder")
  → WebhookDispatcher.dispatch(config, event)
```

- **`IntegrationRegistry`** maps trigger event names to `IntegrationConfig` entries (built once at tenant registration from specs + deployment config).
- **`WebhookDispatcher`** handles HTTP dispatch with configurable timeout and retry with exponential backoff.
- **`IntegrationEngine`** runs as a background tokio task, receives `IntegrationEvent` messages via an `mpsc` channel, and dispatches to all registered webhooks for each trigger concurrently.

### Deployment Configuration

Webhook URLs are deployment-specific and live outside the IOA spec. See `reference-apps/ecommerce/integration.toml`:

```toml
[[webhook]]
name = "notify_fulfillment"
url = "https://fulfillment.example.com/orders"
method = "POST"
timeout_ms = 5000
max_retries = 3
```

Each entry specifies the HTTP endpoint, method, timeout, and retry policy.

### Key Design Decisions

- **Not inline in the state machine.** Integrations are side effects, not transitions. The verification cascade (L0-L3) works on the pure state machine unchanged.
- **At-least-once delivery.** Trigger events originate from the Postgres event journal, so they survive crashes.
- **Retry with exponential backoff.** Configurable per integration via `RetryPolicy`.
- **DST-safe.** Deterministic simulation ignores `EmitEvent` effects — no HTTP calls during testing.

---

## 10. Evolution Engine

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

## 11. Trajectory Intelligence

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
POST /tdata/$feedback
{
    "TraceId": "abc-123",
    "Score": 0.8,
    "Signal": "task_completed",
    "Comment": "worked but took an extra step"
}
```

---

## 12. JIT Optimization

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

## 13. Performance Characteristics

Benchmarks run through the full OData HTTP API stack: HTTP request → axum routing → OData path parsing → Cedar authz → actor dispatch → TransitionTable evaluation → Postgres event persistence → JSON response serialization.

### Latency by Layer

| Layer | Latency | Notes |
|-------|---------|-------|
| `evaluate_ctx()` (rule index lookup) | **~28ns** | BTreeMap O(log K), zero allocation |
| EvalContext construction | ~150ns | BTreeMap inserts for counters + booleans |
| TransitionTable compilation | ~16μs | `from_ioa_source()` — parse + build + index |
| Actor dispatch (in-memory) | ~28μs | Spawn actor + send message + evaluate + respond |
| Actor dispatch (with Postgres) | ~1.7ms | Dominated by Postgres event append |
| Full agent checkout (13 actions, in-memory) | ~340μs | Order + Payment + Shipment lifecycle |
| Full agent checkout (13 actions, Postgres) | **~18ms** | The realistic end-to-end number |

### Concurrency — Full OData HTTP Stack + Postgres

| Scenario | Latency | Throughput |
|----------|---------|------------|
| 1 agent checkout (13 actions) | ~18ms | ~55 checkouts/sec |
| 10 concurrent checkouts | ~62ms | ~160 checkouts/sec |
| 100 concurrent checkouts | ~591ms | ~170 checkouts/sec |

100 concurrent checkouts = 1,300 OData HTTP requests across 300 entity actors, all persisted to Postgres. Throughput: **~2,200 persisted actions/sec** through the full stack.

### Bottleneck Hierarchy

Postgres I/O dominates at ~1.4ms per persisted action write — **50x slower** than the in-memory compute path (~28μs). The `evaluate_ctx()` hot path at 28ns is effectively free. Optimization priorities:

1. **Postgres write batching** — batch multiple events per round-trip
2. **Connection pooling** — already uses sqlx pool (max_connections=10)
3. **Read path caching** — actors rebuild state from journal; snapshots reduce replay cost

### Running Benchmarks

```bash
# TransitionTable micro-benchmarks (always works)
cargo bench -p temper-jit --bench table_eval

# Server actor dispatch overhead
cargo bench -p temper-server --bench actor_throughput

# Realistic e-commerce agent checkout (through full OData HTTP stack)
cargo bench -p ecommerce-reference --bench agent_checkout

# With Postgres persistence (requires running Postgres)
DATABASE_URL=postgres://user:pass@localhost/db cargo bench -p ecommerce-reference --bench agent_checkout
```

### Telemetry Verification

All actions emit OTEL traces to ClickHouse via the OTLP collector. Verify with:

```sql
SELECT SpanName, Duration / 1000000 as duration_ms
FROM otel_traces
WHERE Timestamp > now() - INTERVAL 5 MINUTE
ORDER BY Timestamp
```

---

## 14. API Reference Quick Guide

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

## 15. Common Workflows

### Compile-First Onboarding (Coding Agent Path)

The recommended path for coding agents that generate specs:

1. Write `model.csdl.xml` with entity types, entity sets, and actions
2. Write `*.ioa.toml` for each entity with states, actions, guards, invariants
3. Run `temper verify --specs-dir specs` — all 4 cascade levels must pass
4. Run `temper serve --specs-dir specs --tenant my-app`
5. The OData API is now live at `/tdata`
6. Use `X-Tenant-Id: my-app` header to target your tenant

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

### Deployment-Ready Checklist

Before deploying or handing off a Temper project, verify every item. Skipping
any of these causes silent failures that are hard to diagnose after the fact.

**Specs and Verification:**
- [ ] All `*.ioa.toml` specs pass `temper verify` (4-level cascade: L0-L3)
- [ ] `model.csdl.xml` entity types match IOA spec names and states
- [ ] Cedar policies exist for each entity type in `specs/policies/`
- [ ] No warnings from L0 SMT (dead guards, unreachable states)
- [ ] If specs contain `[[integration]]` sections, `integration.toml` exists with webhook URLs and retry config

**Persistence (events survive restart):**
- [ ] `DATABASE_URL` is set and points to a running Postgres instance
- [ ] Server startup log shows "Postgres connected, migrations applied"
- [ ] Server startup log does NOT show "running in-memory only"
- [ ] After creating an entity and dispatching an action, query Postgres to confirm:
  ```sql
  SELECT event_type, payload->>'from_status', payload->>'to_status'
  FROM events ORDER BY created_at DESC LIMIT 5;
  ```
  If this returns rows, persistence is working. If the table doesn't exist or
  is empty after actions, something is wrong.

**Telemetry (spans and metrics export):**
- [ ] `OTLP_ENDPOINT` is set (e.g., `http://localhost:4318`) if telemetry is wanted
- [ ] OTEL Collector is running and reachable at that endpoint
- [ ] Server startup does not show OTEL initialization errors
- [ ] If running under Claude Code or other OTEL-injecting tools, verify
      telemetry is going to YOUR collector, not the tool's (see Appendix D)

**Infrastructure (Docker Compose):**
- [ ] Postgres volume mounted at `/var/lib/postgresql` (NOT `/var/lib/postgresql/data`)
- [ ] OTEL Collector config mounted at `/etc/otelcol-contrib/config.yaml` (contrib image)
- [ ] ClickHouse exporter uses `tcp://` protocol (port 9000), not `http://`
- [ ] All services show `healthy` in `docker compose ps`
- [ ] Redis: **not required for single-node deployment**. The Docker Compose
      includes Redis for future distributed features, but the server does not
      connect to it. Do not set `REDIS_URL` expecting it to do anything — actor
      mailboxes are local `mpsc` channels, not Redis streams.

**Runtime behavior:**
- [ ] `GET /tdata` returns the service document listing all entity sets
- [ ] `GET /tdata/$metadata` returns valid CSDL XML
- [ ] Creating an entity returns 201 with `status` matching the initial state
- [ ] Dispatching a valid action returns 200 with updated status
- [ ] Dispatching an invalid action (wrong state) returns 409 Conflict
- [ ] Guard enforcement works: e.g., SubmitOrder with 0 items returns 409

**Multi-tenancy (if applicable):**
- [ ] `X-Tenant-Id` header routes to the correct tenant
- [ ] Different tenants see only their own entities

---

## 16. Anti-Patterns to Avoid

| Anti-Pattern | Why It's Wrong | Do This Instead |
|-------------|---------------|-----------------|
| Hand-editing generated code | Will be overwritten on next codegen | Modify the CSDL/automaton specs |
| Using any constructor other than `from_ioa_source()` | TLA+ path fully removed, only IOA is supported | Use `TransitionTable::from_ioa_source()` |
| Skipping verification | Deploys unverified state machines | Always run `temper verify` |
| Skipping `temper verify` before `temper serve --specs-dir` | Server now runs cascade at startup, but pre-verifying catches errors earlier | Always run `temper verify` first |
| Calling actions without checking status | Will get 409 Conflict | GET entity first, check `status` field |
| Calling actions without reading $metadata | May attempt invalid transitions | Read Agent.Hint annotations first |
| Hard-coding entity URLs | Breaks if entity set names change | Read service document at `/tdata` first |
| Writing provider-specific observability queries | Breaks when swapping Logfire↔Datadog | Use canonical SQL schema (spans, logs, metrics) |
| Modifying Cedar policies without human approval | Security change requires human gate | Submit as evolution A-Record for review |
| Creating evolution records without evidence | Unverifiable claims | Include SQL evidence queries in O-Records |
| Putting guards inline in action params | Guards are separate from action parameters | Use `guard = "items > 0"` for preconditions, `params = [...]` for action inputs |
| Deploying without `DATABASE_URL` | Server runs fine but events are lost on restart — silent data loss | Always set `DATABASE_URL`, verify "Postgres connected" in startup log |
| Not querying Postgres after first deploy | No way to know if persistence is actually working | Run `SELECT COUNT(*) FROM events` after dispatching actions |
| Putting webhook calls inside state machine guards or effects | Breaks deterministic verification, introduces network into the transition | Use `[[integration]]` declarations — external calls happen out-of-band via the Integration Engine |

---

## 17. Conversational Vision: How to Represent Temper to Users

When a coding agent (you) uses Temper to build applications for a developer, PM,
or founder, follow these principles for how you communicate.

### The User Does Not Need to Know the Internals

Temper is like a compiler. The user writes what they want; the internals are hidden.
A PM building a project tracker does not need to know about:
- IOA TOML syntax or `[[invariant]]` sections
- Z3 SMT formulas, Stateright BFS, or proptest shrinking
- `TemperModel`, `TransitionTable`, `EvalContext`, or `ModelGuard`
- State counts, guard satisfiability, or induction proofs

**Default behavior: show only the result.** "Verified and live" or a plain-language
explanation of what went wrong.

### Progressive Disclosure (EXPLAIN ANALYZE Analogy)

Like PostgreSQL's `EXPLAIN ANALYZE` or a compiler's `-v` flag, internals are
available on demand — but never the default:

| Level | Trigger | What the user sees |
|-------|---------|-------------------|
| **Result only** (default) | Any spec change | "✓ Verified" or domain-level error explanation |
| **Spec summary** | "Show me what you generated" | Entity structure in plain language (states, actions, rules) |
| **Cascade details** | "Show me verification details" | L0-L3 results, state counts, counterexample traces |

### When Verification Fails, Explain the Domain Problem

Never show the formal violation. Translate it:

| Internal violation | What you say to the user |
|-------------------|--------------------------|
| `NoFurtherTransitions` invariant violated | "You said cancelled is final, but this change adds a transition out of Cancelled" |
| `CounterPositive` invariant violated | "An order can reach Submitted with zero items — should submission require at least one item?" |
| `BoolRequired` invariant violated | "An order can be shipped without payment being captured — should shipping require payment?" |
| Guard unsatisfiable (dead code) | "The action 'Go' can never fire because items can never reach 10 with the current limits" |
| Unreachable state | "The state 'Deploying' can never be reached from the initial state — is it needed?" |

### Two Interaction Patterns

**Interactive mode.** Go back and forth with the user, one entity at a time.
Each exchange that implies a change triggers generation + verification immediately.

**Plan mode.** The user describes everything upfront. Generate all specs at once,
verify, deploy. Follow-up conversation refines incrementally.

### Two-Context Separation

Always maintain this separation when building for users:

1. **Developer Chat** (design-time): The user builds and evolves the application.
   You (the agent) generate specs, verify, and deploy. The user controls *what*
   the app does without needing to understand *how* verification works.

2. **Production Chat** (runtime): End users interact with the deployed app.
   The production agent operates strictly within current specs — it cannot modify
   the entity model. When users attempt something that doesn't exist, capture
   the intent and surface it to the developer:
   "Users are asking to split orders (47 attempts this week). Should I add that?"

   The developer retains the approval gate for all behavioral changes.

---

## 17. TigerStyle Engineering Philosophy (Internal)

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

If a spec change passes the 4-level verification cascade, it ships. If it doesn't, the cascade tells you exactly why and what to fix. There's no "we'll fix the invariant violation later" — the cascade is a hard gate.

---

## 18. DST-First Development Methodology (Internal)

When adding new features or changing state machines, follow the **DST-first** approach:

1. **Write the automaton spec change** (add states, actions, invariants)
2. **Write DST tests** that exercise the new behavior through the actor system:
   ```rust
   #[tokio::test]
   async fn dst_new_feature() {
       let system = ActorSystem::new("dst");
       let table = Arc::new(TransitionTable::from_ioa_source(MY_IOA));
       let actor = EntityActor::new("Order", "test-1", table, json!({}));
       let actor_ref = system.spawn(actor, "test-1");
       // Exercise the new transition...
   }
   ```
3. **Run DST tests** — they will fail if guards are wrong, transitions are missing, or invariants are violated
4. **Fix bugs found by DST** — these are real bugs that would manifest in production
5. **Run the full verification cascade** (`temper verify`) to prove correctness at all 4 levels
6. **Wire into HTTP** — the same TransitionTable that passes DST runs in the entity actors

This approach caught three real bugs during Temper's own development:
- `SubmitOrder` succeeding with 0 items (guard not enforced)
- `CancelOrder` missing entirely from the transition table (filtered as a guard predicate)
- Guard predicates not detected for parameterized actions like `CancelOrder(reason)`

All three were found by DST tests before any HTTP request was made.

---

## Appendix A: Infrastructure Setup

The canonical example is `reference-apps/ecommerce/` — start there for a working setup.

### Quick Start

```bash
# Start Postgres, Redis, ClickHouse, OTEL Collector
docker compose up -d

# Start server with persistence + OTEL telemetry
DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper \
OTLP_ENDPOINT=http://localhost:4318 \
cargo run -p ecommerce

# Start the conversational developer platform
temper serve --dev

# Start in production mode with pre-built specs
temper serve --production --specs-dir specs --tenant my-app
```

### Docker Compose Template

When creating a new `docker-compose.yml`, use these settings:

```yaml
services:
  postgres:
    image: postgres:18-alpine
    environment:
      POSTGRES_DB: myapp
      POSTGRES_USER: myapp
      POSTGRES_PASSWORD: myapp_dev
    ports:
      - "5432:5432"
    volumes:
      # IMPORTANT: Postgres 18 changed its data directory layout.
      # Mount at /var/lib/postgresql (NOT /var/lib/postgresql/data).
      - pg_data:/var/lib/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U myapp"]
      interval: 2s
      timeout: 5s
      retries: 10

  clickhouse:
    image: clickhouse/clickhouse-server:latest
    environment:
      CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT: 1
      # IMPORTANT: Set both USER and PASSWORD, even if empty.
      CLICKHOUSE_USER: default
      CLICKHOUSE_PASSWORD: ""
    ports:
      - "8123:8123"
      - "9000:9000"
    volumes:
      - ch_data:/var/lib/clickhouse

  otel-collector:
    image: otel/opentelemetry-collector-contrib:latest
    ports:
      - "4317:4317"
      - "4318:4318"
    volumes:
      # IMPORTANT: The contrib image reads from /etc/otelcol-contrib/,
      # NOT /etc/otelcol/. Using the wrong path silently ignores your config.
      - ./otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro
    depends_on:
      clickhouse:
        condition: service_healthy

volumes:
  pg_data:
  ch_data:
```

### OTEL Collector Config Template

The collector receives OTLP/HTTP from Temper and exports to ClickHouse via native TCP. Key settings:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: "0.0.0.0:4318"

exporters:
  clickhouse:
    # IMPORTANT: Use tcp:// (native protocol on port 9000), NOT http://
    endpoint: tcp://clickhouse:9000?dial_timeout=10s&compress=lz4
    database: default
    username: default
    password: ""
    traces_table_name: otel_traces
    metrics_table_name: otel_metrics
    logs_table_name: otel_logs
    ttl: 72h
    # IMPORTANT: Let the exporter create tables automatically.
    create_schema: true
```

See `scripts/otel-collector.yaml` or `reference-apps/ecommerce/otel-collector-config.yml` for complete configs.

### .env Template

```bash
DATABASE_URL=postgres://myapp:myapp_dev@localhost:5432/myapp
OTLP_ENDPOINT=http://localhost:4318
CLICKHOUSE_URL=http://localhost:8123
ANTHROPIC_API_KEY=sk-ant-...
RUST_LOG=info,temper=debug

# Not used yet (reserved for future distributed deployment):
# REDIS_URL=redis://localhost:6379
```

### Running Under Claude Code or Other OTEL-Injecting Tools

Claude Code, Datadog agents, and similar tools inject `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and related env vars. These signal-specific vars take precedence over the generic `OTEL_EXPORTER_OTLP_ENDPOINT` in the OTEL SDK, silently routing telemetry to the wrong backend.

**`init_tracing()` handles this automatically** — it clears all signal-specific OTEL env vars before setting the generic endpoint. If you're running outside Temper's `init_tracing()`:

```bash
env -u OTEL_EXPORTER_OTLP_TRACES_ENDPOINT \
    -u OTEL_EXPORTER_OTLP_METRICS_ENDPOINT \
    -u OTEL_EXPORTER_OTLP_LOGS_ENDPOINT \
    -u OTEL_EXPORTER_OTLP_PROTOCOL \
    OTLP_ENDPOINT=http://localhost:4318 \
    cargo run -p my-app
```

## Appendix B: CLI Reference

```
temper init <name>              Create a new Temper project
temper codegen [--specs-dir DIR] [--output-dir DIR]
                                Generate Rust code from specs
temper verify [--specs-dir DIR] Run 4-level verification cascade
temper serve [--port PORT] [--specs-dir DIR] [--tenant NAME]
                                Start platform server
                                With --specs-dir: runs verification cascade, loads specs, serves tenant
                                Without --specs-dir: empty registry, system tenant only
```

## Appendix C: Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `DATABASE_URL` | For persistence | Postgres connection string. **Without this, events are lost on restart.** |
| `REDIS_URL` | Not used | Reserved for future distributed deployment. Server does not read this. |
| `OTLP_ENDPOINT` | For telemetry export | OTLP collector base URL (e.g., `http://localhost:4318`) |
| `CLICKHOUSE_URL` | For analysis queries | ClickHouse HTTP endpoint (read path for trajectory analysis) |
| `ANTHROPIC_API_KEY` | For agent mode | Claude API key |
| `RUST_LOG` | No | Log level (default: `info,temper=debug`) |

**OTEL env var precedence:** The OTEL SDK reads signal-specific env vars (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`) *before* the generic `OTEL_EXPORTER_OTLP_ENDPOINT`. If any signal-specific var is set — even by an unrelated tool — it silently overrides Temper's configured endpoint for that signal. `init_tracing()` clears these vars automatically. See Appendix D for details.

## Appendix D: Infrastructure Pitfalls

Known issues that cause silent failures. All are fixed in the framework, but documented here for awareness.

### 1. OTEL Env Var Collision

**Symptom:** Telemetry silently goes to the wrong endpoint. No errors in logs.

**Root cause:** Tools like Claude Code and Datadog agents set `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. The OTEL SDK reads signal-specific vars before the generic `OTEL_EXPORTER_OTLP_ENDPOINT`, so the tool's endpoint wins.

**Fix:** `init_tracing()` in `temper-observe` clears all signal-specific OTEL env vars before setting the generic endpoint. For manual runs, use `env -u OTEL_EXPORTER_OTLP_TRACES_ENDPOINT ...` (see Appendix A).

### 2. OTEL Collector Config Mount Path

**Symptom:** Collector starts but ignores your config. Uses built-in defaults (no ClickHouse exporter).

**Root cause:** `otel/opentelemetry-collector-contrib` reads from `/etc/otelcol-contrib/config.yaml`, NOT `/etc/otelcol/config.yaml`. The base image (`otel/opentelemetry-collector`) uses `/etc/otelcol/`. Mounting to the wrong path is silently ignored.

**Fix:** Always mount to `/etc/otelcol-contrib/config.yaml` when using the `-contrib` image.

### 3. Postgres 18 Volume Path

**Symptom:** Postgres container crashes on first start with fresh volumes. Errors about `initdb` or data directory permissions.

**Root cause:** Postgres 18 changed its data directory layout. The traditional mount at `/var/lib/postgresql/data` fails on fresh volumes because Postgres 18 expects to create its own `data` subdirectory.

**Fix:** Mount volumes at `/var/lib/postgresql` (not `/var/lib/postgresql/data`).

### 4. ClickHouse Exporter Protocol

**Symptom:** OTEL Collector logs connection errors to ClickHouse. Traces never appear.

**Root cause:** The ClickHouse exporter uses the native TCP protocol (port 9000), not HTTP (port 8123). Using `http://clickhouse:8123` fails with protocol errors.

**Fix:** Use `tcp://clickhouse:9000?dial_timeout=10s&compress=lz4` in the collector config. Set `create_schema: true` to let the exporter create tables automatically.
