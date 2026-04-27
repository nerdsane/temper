# Temper — Personal Notes

## What is Temper?

A platform where you describe an application through conversation, and the system builds and runs it.

- Developer talks to the system: "I want a project management tool with tasks..."
- System generates formal specs (IOA + CSDL + Cedar)
- Specs get mathematically verified
- App is deployed — no hand-written application code
- **Specs are the source of truth, not code**

## Two Worlds

### Design-Time (developer shapes the app)

Conversation generates three artifacts:

- **IOA specs** (`.ioa.toml`) — entity state machines: states, actions, guards, transitions
- **CSDL** (`.csdl.xml`) — data model: fields, relationships
- **Cedar policies** (`.cedar`) — authorization: who can do what

These go through a **verification cascade** (L0-L3) before anything runs.

### Runtime (users use the app)

Verified specs compile into TransitionTables. Entity actors enforce the rules. Users interact through an auto-generated OData API.

## Request Flow

```
POST /tdata/Orders('42')/submit
              │
              ▼
┌──────────── URL PARSING ────────────┐
│ entity set: "Orders"                │
│ key:        "42"                    │
│ action:     "submit"               │
│ tenant:     from X-Tenant-Id header │
└──────────────┬──────────────────────┘
               ▼
┌──────────── SPEC REGISTRY ──────────┐
│ (tenant, "Order") → TransitionTable │
│ "Orders" plural  → "Order" type    │
└──────────────┬──────────────────────┘
               ▼
┌──────────── AUTHORIZATION ──────────┐
│ Cedar policy check:                 │
│ Can this caller "submit" this Order?│
└──────────────┬──────────────────────┘
               ▼
┌──────────── ACTOR LOOKUP ───────────┐
│ Is Order:42 actor alive?            │
│  YES → route to its mailbox        │
│  NO  → spawn actor                 │
│        → load TransitionTable      │
│        → replay events from Postgres│
│        → register in placement map │
└──────────────┬──────────────────────┘
               ▼
┌──────────── TRANSITION EVAL ────────┐
│ table.evaluate("Draft", "submit")   │
│ Guard: item_count > 0 → pass/fail  │
└──────┬───────────────────┬──────────┘
       │                   │
    PASS                 FAIL
       │                   │
       ▼                   ▼
 State → Submitted    409 Conflict
 Persist event        "guard failed"
 Emit telemetry
       │
       ▼
 200 OK + new state
```

## Example: What a Spec Looks Like

From `reference-apps/ecommerce/specs/order.ioa.toml`:

```toml
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Processing",
          "Shipped", "Delivered", "Cancelled",
          "ReturnRequested", "Returned", "Refunded"]
initial = "Draft"

# --- State Variables ---
[[state]]
name = "items"
type = "counter"
initial = "0"

# --- Actions ---
[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]             # only allowed from Draft
to = "Submitted"             # transitions to Submitted
guard = "items > 0"          # must have at least one item
params = ["ShippingAddressId", "PaymentMethod"]

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted", "Confirmed"]  # allowed from 3 states
to = "Cancelled"

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]             # no `to` → stays in Draft
params = ["ProductId", "Quantity"]

# --- Safety Invariants ---
[[invariant]]
name = "SubmitRequiresItems"
when = ["Submitted", "Confirmed", "Processing", "Shipped", "Delivered"]
assert = "items > 0"

[[invariant]]
name = "CancelledIsFinal"
when = ["Cancelled"]
assert = "no_further_transitions"
```

## How the TransitionTable Evaluates

The IOA spec compiles into a `TransitionTable` — a list of `TransitionRule` structs:

```
TransitionTable {
    entity_name: "Order",
    initial_state: "Draft",
    states: ["Draft", "Submitted", ...],
    rules: [
        TransitionRule {
            name: "SubmitOrder",
            from_states: ["Draft"],
            to_state: Some("Submitted"),
            guard: Guard::ItemCountMin(1),    # items >= 1
            effects: [SetState("Submitted"), EmitEvent("SubmitOrder")]
        },
        TransitionRule {
            name: "CancelOrder",
            from_states: ["Draft", "Submitted", "Confirmed"],
            to_state: Some("Cancelled"),
            guard: Guard::Always,             # no guard
            effects: [SetState("Cancelled"), EmitEvent("CancelOrder")]
        },
        ...
    ],
    rule_index: { "SubmitOrder" → [0], "CancelOrder" → [1], ... }
}
```

### Interface

You give it: **where I am** + **what I have** + **what I want to do**. It decides everything else.

```rust
table.evaluate_ctx(
    current_state: &str,   // "Draft"
    ctx: &EvalContext,     // arbitrary counters, booleans, lists
    action: &str,          // "SubmitOrder"
) → Option<TransitionResult>
```

The `EvalContext` is fully generic — nothing use-case specific:

```rust
EvalContext {
    counters: BTreeMap<String, usize>,       // "items": 2, "retries": 1, ...
    booleans: BTreeMap<String, bool>,        // "has_address": true, "is_paid": false, ...
    lists:    BTreeMap<String, Vec<String>>, // "tags": ["urgent"], "approvers": ["alice"], ...
}
```

| Return | Meaning |
|--------|---------|
| `Some({ success: true })` | Action allowed, here's the new state + effects |
| `Some({ success: false })` | Action exists but guard failed (e.g. items == 0) |
| `None` | Action doesn't exist at all |

The caller never says "go to Submitted" — the table encodes that.

### What is generated vs what is generic code

```
order.ioa.toml ──compile──→ TransitionTable { rules: [SubmitOrder, CancelOrder, ...] }
ticket.ioa.toml ──compile──→ TransitionTable { rules: [AssignTicket, CloseTicket, ...] }
                                      │
                                      │  same evaluate_ctx() function
                                      │  for both — it's generic, never changes
                                      ▼
                              table.evaluate_ctx(state, ctx, action)
```

- **Spec** → generates **data** (the rules list inside the TransitionTable)
- **Code** (`evaluate_ctx`) → never changes, ships in `temper-jit`, works on any TransitionTable
- There is NO per-entity code. The only thing that varies is the rules data.

### Evaluation algorithm (`evaluate_ctx`)

```
evaluate_ctx(current_state="Draft", ctx={items: 2}, action="SubmitOrder"):

1. rule_index.get("SubmitOrder") → [0]        # O(log K) lookup
2. rules[0].from_states = ["Draft"]
   → "Draft" in ["Draft"]? YES                # state check
3. rules[0].guard = ItemCountMin(1)
   → ctx.items(2) >= 1? YES                   # guard check
4. Return TransitionResult {
       new_state: "Submitted",
       effects: [SetState("Submitted"), EmitEvent("SubmitOrder")],
       success: true
   }
```

If the guard fails → returns `success: false`, state unchanged.
If the action is unknown → returns `None`.

### Guard types

| Guard | Meaning |
|-------|---------|
| `Always` | No guard, always passes |
| `StateIn(["Draft"])` | Current state must be in set |
| `ItemCountMin(1)` | items counter >= N |
| `CounterMin { var, min }` | Named counter >= N |
| `CounterMax { var, max }` | Named counter < N |
| `BoolTrue("has_address")` | Named boolean must be true |
| `And([guard1, guard2])` | All inner guards must pass |
| `CrossEntityStateIn { .. }` | Another entity must be in required status |

### Effect types

| Effect | Meaning |
|--------|---------|
| `SetState("Submitted")` | Change entity status |
| `IncrementItems` / `DecrementItems` | +1 / -1 items counter |
| `IncrementCounter("retries")` | +1 named counter |
| `SetBool { var, value }` | Set boolean variable |
| `EmitEvent("SubmitOrder")` | Emit event to telemetry |
| `ScheduleAction { action, delay }` | Timer: fire action after N seconds |
| `SpawnEntity { .. }` | Create a child entity |
| `Custom("NotifyAdmin")` | Domain-specific hook |

## Actor Lifecycle: Spawn → Replay → Handle → Persist

### 1. Spawn (actor doesn't exist yet)

```
Request for Order:42 arrives
        │
        ▼
ServerState checks: is Order:42 actor alive?
        │ NO
        ▼
Spawn EntityActor {
    tenant: "acme",
    entity_type: "Order",
    entity_id: "42",
    table: Arc<RwLock<TransitionTable>>,  ← shared with all Order actors
    event_store: Some(postgres),
}
```

### 2. Replay (rebuild state from event history)

```
pre_start() is called
        │
        ▼
Read events from Postgres for "acme:Order:42"
        │
        ▼
Events in DB:
  seq 1: { action: "Created",     to_status: "Draft" }
  seq 2: { action: "AddItem",     to_status: "Draft",     params: {ProductId: "P1"} }
  seq 3: { action: "AddItem",     to_status: "Draft",     params: {ProductId: "P2"} }
        │
        ▼
For each event:
  - Build EvalContext from current state
  - Re-evaluate through TransitionTable (same code as live handling)
  - Apply effects to rebuild counters, booleans, status
        │
        ▼
Rebuilt state:
  status: "Draft"
  items: 2
  sequence_nr: 3
  events: [Created, AddItem, AddItem]
```

Key: replay uses the **same TransitionTable evaluation** as live requests.
Same input → same output. Deterministic.

### 3. Handle (process incoming action)

```
EntityMsg::Action { name: "SubmitOrder", params: {...} }
        │
        ▼
Build EvalContext { counters: {"items": 2}, booleans: {...} }
        │
        ▼
table.evaluate_ctx("Draft", ctx, "SubmitOrder")
        │
        ▼
TransitionResult { new_state: "Submitted", success: true,
                   effects: [SetState("Submitted"), EmitEvent("SubmitOrder")] }
        │
        ▼
Apply effects to state:
  state.status = "Submitted"
  state.fields["Status"] = "Submitted"
```

### 4. Persist (store event to Postgres)

```
EntityEvent {
    action: "SubmitOrder",
    from_status: "Draft",
    to_status: "Submitted",
    params: { ShippingAddressId: "A1", PaymentMethod: "card" },
    timestamp: "2024-01-15T10:30:00Z",
}
        │
        ▼
event_store.append("acme:Order:42", sequence_nr=3, [envelope])
        │
        ▼
sequence_nr: 3 → 4
        │
        ▼
Return EntityResponse { success: true, state: {...} } → 200 OK
```

### Persistence ID format

```
"{tenant}:{entity_type}:{entity_id}"
  "acme:Order:42"
```

This is the key used in both Postgres (event journal) and Redis (placement).

## Key Concepts

| What | Description | Cardinality |
|------|-------------|-------------|
| **TransitionTable** | Compiled spec rules | One per (tenant, entity type) |
| **EntityActor** | Holds instance state, processes messages via bounded mailbox | One per entity instance |
| **SpecRegistry** | Maps (tenant, entity type) → TransitionTable + metadata | One global |
| **Tenant** | Isolated customer/workspace (like a Slack workspace) | Many per server |

## Important Properties

- **Actor is generic** — zero business logic, just evaluates the TransitionTable
- **Business logic lives in the spec** — encoded in the TransitionTable
- **Event sourcing** — Postgres stores event history, current state rebuilt by replay
- **Hot-swap** — new spec → new TransitionTable swapped via `Arc<RwLock>`, actors pick it up on next action without restart
- **Multi-tenant** — same server, different specs per tenant, fully isolated

## Crate Map

```
temper-spec ──────────┬──→ temper-verify (design-time: model checking, DST, proptest)
                      │
                      └──→ temper-jit ──→ temper-runtime ──→ temper-server
                                               │                  │
                                               │                  ├──→ temper-store-postgres (events)
                                               │                  └──→ temper-store-redis (placement)
                                               │
                                               └──→ temper-observe (telemetry)

temper-evolution — O-P-A-D-I record chain (unmet intents → spec proposals)
temper-platform  — deploy pipeline, shared SpecRegistry orchestration
temper-authz     — Cedar policy engine wrapper
temper-mcp       — MCP server for agent integration
temper-wasm      — WebAssembly module runtime (for logic specs can't express)
temper-cli       — Developer CLI (verify, serve, codegen)
```

## Evolution Engine — O-P-A-D-I-FR Record Chain

Every change to the system has an auditable evidence trail. Records are immutable, timestamped, and linked via `derived_from`.

```
O → P → A → D → I → FR
│   │   │   │   │   │
│   │   │   │   │   └─ Feature Request: platform gap, needs dev review
│   │   │   │   │
│   │   │   │   └─ Insight: "users keep trying X, build it"
│   │   │   │
│   │   │   └─ Decision: human approves/rejects/defers
│   │   │
│   │   └─ Analysis: root cause + proposed spec diffs
│   │
│   └─ Problem: formal problem statement (Lamport-style)
│
└─ Observation: anomaly detected in production
```

### Concrete Example

1. **O** — Sentinel detects latency spike: *"p99 is 450ms, threshold is 100ms"*
2. **P** — System formulates: *"Order processing exceeds SLO, 189 users affected, trend growing"*
3. **A** — Root cause found: *"shard key causes hotspot"*, proposes spec diff + risk assessment
4. **D** — Human reviews: *"Approved — low risk"* → verification cascade runs → codegen + deploy
5. **I** — Trajectory analysis: *"234 users tried 'split order', 18% success, growing"* → tells developer what to build next
6. **FR** — Platform gap: *"agents called 'send_email' 47 times — method doesn't exist"*

### Key Properties

- Each record has an ID with type prefix: `O-2024-0042`, `P-2024-0043`, etc.
- Chain is validated: D must derive from A, A from P, P from O
- Nothing changes without all links in the chain
- **Observation** created by SentinelActors watching telemetry
- **Problem** follows Lamport's method: state the problem precisely before solving
- **Analysis** includes spec diffs, risk level, TLA+ impact
- **Decision** is always human — approve, reject, or defer
- **Insight** is product intelligence — unmet intents, friction, workarounds
- **Feature Request** captures platform-level gaps (missing methods, blocked governance)

## Infrastructure (docker-compose)

- **Postgres** — event store (port 5432)
- **Redis** — actor placement cache + mailboxes (port 6379)
- **ClickHouse** — trajectory/telemetry aggregation (port 8123)
- **OTEL Collector** — spans + metrics pipeline (ports 4317/4318)

---

## Verification Cascade (Design-Time)

### Where it fits

The cascade is a **gate** between spec generation and deployment. Nothing reaches runtime without passing.

```
Developer conversation
        │
        ▼
System generates spec (.ioa.toml + .csdl.xml + .cedar)
        │
        ▼
Verification cascade (L0 → L1 → L2 → L2b → L3)
        │
     ALL PASS?
     │       │
    YES      NO
     │       │
     ▼       ▼
  Deploy   Reject — go back to
  to       conversation, fix spec
  runtime
```

### The Levels

```
Spec (order.ioa.toml)
         │
         ▼
┌─── L0: Symbolic Verification (Z3 SMT solver) ───┐
│ • Guard satisfiability — can each guard ever fire?│
│   (if UNSAT → dead code, action can never happen) │
│ • Invariant induction — does invariant hold after │
│   every possible transition? (algebraic proof)    │
│ • Unreachable state detection — BFS from initial  │
│ Pure math. No state enumeration.                  │
└──────────────────┬───────────────────────────────┘
                   ▼
┌─── L1: Model Check (Stateright) ─────────────────┐
│ Exhaustive state-space exploration.               │
│ Enumerates EVERY reachable state.                 │
│ Checks safety + liveness properties.              │
│ Reports counterexamples if violated.              │
└──────────────────┬───────────────────────────────┘
                   ▼
┌─── L2: Deterministic Simulation ─────────────────┐
│ FoundationDB/TigerBeetle-style.                   │
│ Multiple actors, fault injection (msg drops, etc) │
│ Multiple seeds (default 10), 200 ticks each.      │
│ Checks invariants + liveness under faults.        │
└──────────────────┬───────────────────────────────┘
                   ▼
┌─── L2b: Actor Simulation ────────────────────────┐
│ Same as L2 but runs through REAL                   │
│ TransitionTable.evaluate_ctx(), not the model.    │
│ Catches divergence between model and runtime code.│
└──────────────────┬───────────────────────────────┘
                   ▼
┌─── L3: Property Tests (proptest) ────────────────┐
│ Random action sequences (default 1000 cases).     │
│ Checks invariants after every step.               │
│ Shrinking: if failure found, minimizes to         │
│ smallest reproducing sequence.                    │
└──────────────────────────────────────────────────┘
```

### What each level catches

| Level | Catches | Technique |
|-------|---------|-----------|
| **L0** | Dead guards, broken invariants | Z3 algebraic proof |
| **L1** | Any reachable state violating a property | Exhaustive enumeration |
| **L2** | Bugs under faults (msg drops, reordering) | Deterministic simulation |
| **L2b** | Divergence between model and real code | Real `evaluate_ctx()` under simulation |
| **L3** | Edge cases in long random sequences | Randomized + shrinking |

### Key properties

- **All levels run independently** — not a pipeline, they all check the same spec from different angles
- **fail_fast mode** — optionally stop at first failure
- **L2b is the bridge** — proves that what the model checker verified is what the actual runtime code does
- Invariants that can't be verified (e.g. referencing undeclared variables) get flagged as **warnings**, not failures
- Lives in `temper-verify` crate — this crate is **design-time only**, never pulled into production builds

## Side Effects: How Real-World Actions Happen

The actor/state machine is pure — it only mutates entity state. Real-world side effects happen **after** a transition succeeds, outside the actor:

```
Transition succeeds (Draft → Submitted)
        │
        ├── Pure effects (actor handles directly)
        │   SetState, IncrementCounter, SetBool, EmitEvent, ...
        │
        └── Side-effect triggers (server handles post-transition)
            │
            ├── Integrations/Webhooks (declared in spec)
            │   [[integration]]
            │   trigger = "SubmitOrder"
            │   type = "webhook"
            │   → HTTP POST to configured URL
            │
            ├── WASM modules (custom code in WebAssembly)
            │   → Can: http_call(), get_secret(), log()
            │   → Governed by Cedar policies
            │   → In prod: real HTTP (ProductionWasmHost)
            │   → In sim: canned responses (SimWasmHost)
            │
            ├── Custom effects (hooks registered at startup)
            │   Effect::Custom("DeploySpecs")
            │   → Dispatched to post-transition hooks
            │
            └── SpawnEntity (create child entities)
                Effect::SpawnEntity { entity_type: "Workflow", ... }
```

## Scope and Limitations

### What Temper produces

- **OData HTTP API** — CRUD + bound actions on entities
- **SSE event stream** — real-time state changes (`/$events`)
- **Webhook triggers** — outbound HTTP calls on transitions

### What it can orchestrate (but not run directly)

- External services via webhooks or WASM HTTP calls
- Anything reachable over HTTP

### What it cannot do

- Run arbitrary background processes
- Deploy containers directly
- Stream processing
- WebSocket servers
- CLI tools

### Mental model

```
Temper app (state machine)                External world
─────────────────────────                 ──────────────
Entity transitions to new state
        │
        └── webhook fires ──HTTP POST──→ Your external service
                                         (YOUR code, not Temper)
                                                │
                                                └── does the real work
                                                    (k8s deploy, send email, etc)
```

Temper is the **orchestrator** — manages state, enforces rules, gates transitions. The "do stuff in the real world" always delegates to external services over HTTP.

**Scoped to: entity-oriented, data-driven HTTP API applications with verified state machines.**
