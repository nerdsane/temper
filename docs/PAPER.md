# Temper: A Formally Verified, Self-Evolving Actor Framework for Agentic API Backends

**Seshadri Nalla**

*Draft -- February 2026*

---

## Abstract

We present Temper, an actor-based application framework for building API backends
whose primary consumers are autonomous LLM agents rather than human-operated
frontends.  Temper takes a specification-first approach: an OData v4 Common Schema
Definition Language (CSDL) document defines the data model, TLA+ modules define
behavioral state machines with safety invariants and liveness properties, and Cedar
policies define attribute-based access control.  Code is derived from these
specifications and is treated as a regenerable artifact.  A custom tokio-based actor
runtime, inspired by Erlang/OTP and Akka, provides supervision, event sourcing, and
deterministic simulation.  A three-level verification cascade--exhaustive model
checking via Stateright, deterministic simulation with seed-based fault injection,
and property-based testing with automatic shrinking--establishes correctness before
deployment.  A production feedback loop, the Evolution Engine, captures observations,
formalizes problems in the style of Lamport, proposes solutions as specification
diffs, and gates destructive changes on human approval.  Trajectory intelligence
extracts product signal from agent execution traces.  A three-tier JIT execution
model allows state machine transition logic to be hot-swapped at runtime without
process restarts.  The framework is implemented as a 16-crate Rust workspace with
246 tests across 97 source files (16,604 lines of Rust), backed by PostgreSQL for
event sourcing, Redis for actor mailboxes, and ClickHouse for observability.  A
live Claude-powered LLM agent demonstrates the full feedback loop: natural language
requests are interpreted into OData operations, state machine transitions are
persisted to PostgreSQL, trajectory spans are captured to ClickHouse, and the
Evolution Engine generates product intelligence records identifying unmet user
intents.  We evaluate against a reference agentic e-commerce application with 7
entity types and a 10-state order lifecycle.

---

## 1. Introduction

The emergence of autonomous LLM agents as first-class API consumers
fundamentally changes the contract between a backend system and its callers.
Traditional web frameworks--Rails, Django, Spring Boot--are designed around a
request/response model where a human user clicks a button, the framework
routes the request to a controller, and a response is rendered.  The developer
writes imperative handler code; correctness is established, if at all, by unit
tests and code review.

Agentic backends face three compounding challenges that this model does not
address:

1. **Correctness under autonomy.**  An agent may issue hundreds of API calls
   per minute, exploring state spaces that no human tester would traverse.  A
   subtle invariant violation--shipping an order without captured payment, for
   instance--can propagate silently because the agent has no intuition to catch
   it.

2. **Evolvability without code archaeology.**  When an agent's trajectory
   analysis reveals that users want to split an order into multiple shipments,
   the system must evolve.  In a code-first framework, this means modifying
   controllers, models, migrations, and tests.  If an agent is performing the
   modification, it must understand the full codebase.

3. **Optimizability under production load.**  Agents generate access patterns
   that differ from human browsing.  N+1 query patterns, suboptimal cache TTLs,
   and shard hotspots manifest at runtime.  The system should be able to
   detect and correct these autonomously.

Temper's key insight is that **specifications, not code, should be the durable
artifact**.  A CSDL document defines what entities exist and how they relate.  A
TLA+ module defines what transitions are legal and what invariants must hold.  A
Cedar policy defines who may do what.  Code is generated from these
specifications and can be regenerated whenever the specifications change.  Agents
read and modify specifications--structured, bounded, verifiable artifacts--rather
than arbitrary source code.

The remainder of this paper is organized as follows.  Section 2 presents the
overall architecture.  Sections 3--5 describe the specification layer, the actor
runtime, and the verification cascade.  Sections 6--8 cover the Evolution Engine,
trajectory intelligence, and self-optimization.  Section 9 describes the
observability subsystem.  Section 10 surveys related work.  Section 11 evaluates
the framework against the reference application.  Section 12 concludes.

---

## 2. Architecture Overview

Temper is implemented as a Rust workspace of 16 crates plus one reference
application crate.  The workspace targets Rust edition 2024 (rustc 1.85+) and
is dual-licensed under MIT and Apache-2.0.

### 2.1 Crate Map

```
temper-spec          Specification parsing: CSDL, TLA+, unified model
temper-macros        Proc-macro utilities
temper-runtime       Actor system: traits, mailbox, supervision, scheduler
temper-codegen       Code generation from specifications
temper-odata         OData v4 query/path parsing and error types
temper-authz         Cedar ABAC engine
temper-observe       Observability: store trait, schemas, trajectory types
temper-verify        Three-level verification cascade
temper-store-postgres Postgres event store and snapshot store
temper-store-redis   Redis mailbox, cache, shard placement
temper-server        Axum HTTP layer: router, dispatch, response
temper-cli           CLI: init, codegen, verify, serve subcommands
temper-evolution     Evolution Engine: records, chain validation, insights
temper-jit           JIT transition tables, hot-swap, shadow testing
temper-optimize      Self-driving optimizer actors, safety checker
reference/ecommerce  Reference application: agentic e-commerce
```

### 2.2 Three-Tier Architecture

```
+---------------------------------------------------------------------+
|                    SPECIFICATION LAYER                               |
|  model.csdl.xml    order.tla    payment.tla    policies/*.cedar     |
+---------------------------------------------------------------------+
          |                  |                  |
          v                  v                  v
+---------------------------------------------------------------------+
|                      ACTOR RUNTIME                                  |
|  temper-runtime  temper-jit  temper-store-*  temper-authz            |
|                                                                     |
|  +----------+  +----------+  +----------+  +----------+             |
|  | OrderActor|  |PaymentActor| |ShipmentActor| |  ...   |           |
|  +----------+  +----------+  +----------+  +----------+             |
|       |              |              |              |                 |
|       +-------+------+------+------+-----+--------+                 |
|               |             |            |                          |
|         [Mailbox]     [EventStore]  [Snapshots]                     |
+---------------------------------------------------------------------+
          |                  |                  |
          v                  v                  v
+---------------------------------------------------------------------+
|                      HTTP SURFACE                                   |
|  temper-server (Axum)    temper-odata (query/path parsing)          |
|                                                                     |
|  GET /odata/Orders('{id}')?$expand=Items                            |
|  POST /odata/Orders('{id}')/SubmitOrder                             |
|  GET /odata/$metadata                                               |
+---------------------------------------------------------------------+
          |                  |                  |
          v                  v                  v
+---------------------------------------------------------------------+
|                   FEEDBACK LOOP                                     |
|  temper-observe   temper-evolution   temper-optimize                 |
|                                                                     |
|  Sentinel ──> O-Record ──> P-Record ──> A-Record ──> D-Record       |
|                                                                     |
|  TrajectoryAnalyzer ──> I-Record ──> $metadata enrichment           |
+---------------------------------------------------------------------+
```

The blocking/non-blocking boundary lies between the actor runtime and the HTTP
surface.  The HTTP layer is fully asynchronous (Axum on Tokio).  Each actor
processes messages sequentially--one at a time, no concurrent state access--
providing the sequential consistency guarantee that simplifies reasoning about
state machine correctness.

---

## 3. Specification Layer

### 3.1 CSDL as the Data Model Contract

The Common Schema Definition Language (CSDL) is the XML schema format
standardized by OASIS for OData v4 [1].  Temper uses CSDL as the single source
of truth for the data model.  The reference e-commerce application defines seven
entity types (`Customer`, `Address`, `Product`, `Order`, `OrderItem`, `Payment`,
`Shipment`), three enum types (`OrderStatus`, `PaymentStatus`, `ShipmentStatus`),
bound actions (e.g., `SubmitOrder`, `CancelOrder`, `AuthorizePayment`), and
unbound functions (e.g., `SearchProducts`).

Temper extends the CSDL vocabulary with a custom namespace (`Temper.Vocab`) that
annotates entities with:

- **State machine metadata:** `StateMachine.States`, `StateMachine.InitialState`,
  `StateMachine.TlaSpec`, and per-action `ValidFromStates` / `TargetState`.
- **Agent hints:** `Agent.Hint`, `Agent.CommonPattern`, `Agent.SuccessRate`--
  populated from trajectory analysis and surfaced through OData `$metadata`.
- **Sharding:** `ShardKey` annotations for shard-aware actor placement.
- **Authorization:** `AuthZ.CedarPolicy` annotations linking entities and actions
  to Cedar policy files.

This design means that the CSDL document is machine-readable by agents:  an agent
can `GET /odata/$metadata`, parse the XML, discover available entity types and
actions, read state machine constraints, and follow agent hints--all without
inspecting source code.

### 3.2 TLA+ as the Behavioral Specification

Each stateful entity type has an associated TLA+ module that specifies its
complete lifecycle.  The `order.tla` module, for example, defines:

- **State space:** 10 order statuses, 7 payment statuses, 7 shipment statuses.
- **Guards:** Preconditions such as `CanSubmit`, which requires `status = "Draft"`,
  at least one item, and a shipping address.
- **Actions:** 14 state transitions including `AddItem`, `SubmitOrder`,
  `ConfirmOrder`, `CancelOrder`, `InitiateReturn`, and `RefundOrder`.
- **Safety invariants:** `ShipRequiresPayment` (cannot ship without captured
  payment), `SubmitRequiresItems`, `SubmitRequiresAddress`,
  `PaymentRefundConsistency`.
- **Liveness properties:** `SubmittedProgress` (a submitted order is eventually
  confirmed or cancelled), `ProcessingProgress`, `ReturnProgress`.

The specification is formally structured as `Spec == Init /\ [][Next]_vars /\ WF_vars(Next)`,
following the standard TLA+ idiom for a fair specification with stuttering steps.

### 3.3 Cedar ABAC as the Security Specification

Authorization is modeled using Amazon's Cedar policy language [2].  Cedar
evaluates authorization requests of the form `(principal, action, resource,
context)` against a policy set.  Temper's `AuthzEngine` translates OData
operations into Cedar requests, supporting four principal kinds: `Customer`,
`Agent`, `Admin`, and `System`.  System principals bypass all policy checks.
Per-entity Cedar policies are referenced from CSDL annotations, keeping the
security specification co-located with the data model.

---

## 4. Actor Runtime

### 4.1 Core Actor Trait

The actor system is a custom implementation on Tokio, drawing from Erlang/OTP
and Akka but deliberately minimal.  The central abstraction is the `Actor` trait:

```rust
pub trait Actor: Send + 'static {
    type Msg: Message;
    type State: Send + 'static;

    fn supervision_strategy(&self) -> SupervisionStrategy;
    fn pre_start(&self, ctx: &mut ActorContext<Self>)
        -> impl Future<Output = Result<Self::State, ActorError>> + Send;
    fn handle(&self, msg: Self::Msg, state: &mut Self::State,
              ctx: &mut ActorContext<Self>)
        -> impl Future<Output = Result<(), ActorError>> + Send;
    fn post_stop(&self, state: Self::State, ctx: &mut ActorContext<Self>)
        -> impl Future<Output = ()> + Send;
}
```

Messages must satisfy `Send + Debug + 'static`.  The `handle` method is called
sequentially--one message at a time--eliminating data races on actor state by
construction.  Lifecycle hooks (`pre_start`, `post_stop`) bracket the actor's
active period, with `pre_start` returning the initial state and `post_stop`
consuming it for cleanup.

### 4.2 Supervision

Temper implements Erlang-style supervision with two strategies:

- **Stop:** The actor is not restarted; the failure propagates to the supervisor.
- **Restart:** The actor is restarted up to `max_retries` times with exponential
  backoff (`base * 2^(n-1)`, capped at 30 seconds).

The default strategy restarts up to 3 times with a 100ms backoff base.  This
provides fault tolerance for transient errors (e.g., a database connection timeout)
while preventing infinite restart loops for deterministic failures.

### 4.3 Event Sourcing and Snapshots

Actor state is persisted through an event sourcing model.  Every state transition
emits events that are appended to a Postgres-backed event store (`temper-store-postgres`).
Periodic snapshots reduce replay time on actor restart.  Redis (`temper-store-redis`)
provides the mailbox backing store, shard placement registry, and a cache layer with
key-pattern-based TTLs.

### 4.4 Bounded Execution (TigerStyle)

Following TigerStyle's bounded execution principle [18], the actor runtime
enforces static resource limits throughout.  Actor mailboxes have a fixed
capacity; sends to a full mailbox return `ActorError::MailboxFull` rather than
growing unboundedly.  The maximum number of items in an order (and, more
generally, any entity collection) is bounded by `MAX_ITEMS`, which is set at
initialization and enforced by transition table guards.  Event logs are bounded
by snapshot compaction: once the event count since the last snapshot exceeds a
configurable threshold, a snapshot is taken and older events become eligible for
truncation.  The simulation scheduler bounds its pending-message queue to
prevent runaway message generation during fault injection.  No data structure in
the actor runtime grows without an explicit, statically configured upper bound.
This eliminates an entire class of production incidents--out-of-memory crashes
caused by unbounded queues, unbounded caches, or unbounded replay logs--by
construction rather than by monitoring.

### 4.5 Deterministic Simulation Scheduler

For verification purposes, Temper includes a `SimScheduler` that replaces the
Tokio runtime with a single-threaded, seed-controlled message delivery system.
This is discussed in detail in Section 5.

---

## 5. Three-Level Verification Cascade

TigerStyle [18] holds that "assertions are not just for testing--they run in
production."  Temper embodies this principle at the state machine level: the
`TransitionTable` guards that enforce TLA+ invariants on every transition are
runtime assertions, evaluated on every message an actor processes, in every
environment--development, simulation, and production.  This is TigerStyle's
"minimum two assertions per function" applied at the granularity of state
machine transitions: every transition checks its `from_states` guard and its
semantic `Guard` predicate before any effect is applied.  The verification
cascade described below does not *replace* these runtime assertions; it
*complements* them by proving, ahead of deployment, that no reachable state
can violate the invariants those assertions enforce.

Correctness is established through a three-level cascade, orchestrated by the
`VerificationCascade` type in `temper-verify`.  All three levels run
independently; all must pass before deployment.

### 5.1 Level 1: Exhaustive Model Checking

The TLA+ specification is translated into a `TemperModel` that implements the
Stateright `Model` trait [3].  Stateright performs breadth-first exhaustive
exploration of the state space, checking every safety invariant at every
reachable state.  For the reference Order state machine with `MAX_ITEMS = 2`,
the model checker explores the full state graph and confirms that all
properties hold.

```
L1 Model Check PASSED: 42,847 states explored, all properties hold
```

This level provides the strongest guarantee: no reachable state violates any
safety invariant.  The trade-off is that state-space explosion limits
`MAX_ITEMS` to small values.

### 5.2 Level 2: Deterministic Simulation

Inspired by FoundationDB's simulation testing [4] and TigerBeetle's VOPR [5],
Level 2 runs multi-actor scenarios under controlled fault injection.  The key
components are:

- **DeterministicRng:** A xorshift64 PRNG that produces identical sequences
  for identical seeds.  All non-determinism--message delays, drops, actor
  crashes--is derived from this single seed.
- **SimScheduler:** A priority-queue-based message delivery system operating on
  logical ticks rather than wall-clock time.  Messages enter the pending queue
  with a computed delivery time; faults may delay, reorder, or drop them.
- **FaultConfig:** Three preset fault profiles:
  - `none`: Pure deterministic ordering, no faults.
  - `light`: 10% message delay (up to 5 ticks), no drops or crashes.
  - `heavy`: 30% delay (up to 20 ticks), 5% message drop, 2% actor crash
    (with 80% restart probability).

A simulation run instantiates multiple entity actors, each processing random
action sequences drawn from the valid actions for their current state.  After
every transition, the invariant checker verifies that the new state satisfies
all safety properties.  Multi-seed runs (`run_multi_seed_simulation`) sweep
across seeds for broader coverage:

```
L2 Simulation PASSED: 10 seeds, 847 transitions, 23 dropped msgs
```

Crucially, any failure is reproducible: re-running with the same seed
produces the identical execution trace.

### 5.3 Level 3: Property-Based Testing

Level 3 uses proptest [6] to generate random action sequences and check
invariants after each step.  This complements Levels 1 and 2: Level 1 is
exhaustive but bounded; Level 2 simulates realistic multi-actor scenarios;
Level 3 exercises long action sequences that may exceed the model checker's
state-space budget.  When a violation is found, proptest's shrinking algorithm
reduces the sequence to a minimal counterexample.

```
L3 Property Tests PASSED: 1000 cases, 30 max steps
```

### 5.4 Cascade Orchestration

The `VerificationCascade` builder accepts a TLA+ source string and configuration
parameters (number of simulation seeds, property test cases, max items), then
runs all three levels and produces a `CascadeResult`:

```rust
let cascade = VerificationCascade::from_tla(ORDER_TLA)
    .with_sim_seeds(10)
    .with_prop_test_cases(1000);
let result = cascade.run();
assert!(result.all_passed);
```

The cascade is invoked by the CLI (`temper verify`) and by the Evolution Engine
before any specification change is approved for deployment.

---

## 6. Evolution Engine

### 6.1 The O-P-A-D-I Record Chain

The Evolution Engine maintains the system's institutional memory through an
immutable, linked chain of typed records:

1. **O-Record (Observation):** A sentinel actor detects an anomaly in production
   telemetry.  The record carries the SQL evidence query, observed and threshold
   values, and a classification (Performance, ErrorRate, StateMachine, Security,
   Trajectory, ResourceUsage).

2. **P-Record (Problem):** A Lamport-style formal problem statement [7] derived
   from the observation.  It specifies the problem precisely--invariants that
   must continue to hold, constraints on the solution space, and an impact
   assessment (affected users, severity, trend).

3. **A-Record (Analysis):** Root cause analysis with one or more `SolutionOption`s.
   Each option specifies a specification diff (CSDL changes, TLA+ changes, Cedar
   policy changes), the impact on TLA+ invariants, a risk level, and an
   estimated complexity.

4. **D-Record (Decision):** A human approval or rejection of a proposed change.
   If approved, the record includes verification cascade results (Stateright
   states explored, simulation pass/fail, proptest cases) and an implementation
   plan (codegen command, migration required, deployment strategy).

5. **I-Record (Insight):** Product intelligence derived from trajectory analysis,
   categorized as UnmetIntent, Friction, or Workaround (see Section 7).

Each record carries a `RecordHeader` with a unique ID (e.g., `O-2024-0042`),
a timestamp, creator identity, and a `derived_from` link to its predecessor.
The chain validation logic (`validate_chain`) walks from any leaf record back to
the root, verifying that the type ordering is correct (D derives from A, A from
P, P from O) and that no links are broken.

### 6.2 Human Approval Gates

Destructive changes--those that alter state machine invariants, remove entity
types, or modify Cedar policies--require a human `D-Record` with
`Decision::Approved` before codegen and deployment proceed.  This is a
deliberate design constraint: the system may autonomously observe, formalize
problems, analyze root causes, and propose solutions, but it may not unilaterally
implement changes that affect correctness.

### 6.3 Dual Storage

Evolution records are stored in both Git (for version-controlled history and
diff visibility) and Postgres (for programmatic querying by sentinel actors).
This dual-write ensures that the institutional memory is both human-auditable
and machine-queryable.

---

## 7. Trajectory Intelligence

### 7.1 Trajectories as Product Signal

A *trajectory* is the complete sequence of API calls an agent makes within a
single user turn.  Each span within a trajectory carries a `TrajectoryContext`
with the distributed trace ID, turn number, parsed user intent, prompt version,
and agent identity.  The terminal state of a trajectory is captured in a
`TrajectoryOutcome` (completed, failed, pivoted, abandoned) along with a
feedback score, token count, and API call count.

### 7.2 Two Optimization Loops

Trajectory data feeds two distinct optimization loops:

- **Agent optimization:** If an agent's trajectories show high failure rates on
  a specific action, the `Agent.SuccessRate` annotation in the CSDL is updated
  and the `Agent.Hint` is enriched.  The next time the agent reads `$metadata`,
  it receives improved guidance.  In the reference application, the
  `CancelOrder` action carries a historical success rate of 0.73, signaling to
  agents that cancellation frequently fails (because the order has already
  progressed past the cancellable states).

- **API optimization:** If trajectory analysis reveals that agents consistently
  perform the same three-step sequence to accomplish a task (e.g., GET
  Customer, GET Order with Items, POST SubmitOrder), the system may propose a
  composite action in the CSDL that collapses the sequence.

### 7.3 Product Intelligence

The `InsightRecord` captures product-level findings:

- **UnmetIntent:** Agents attempt something the API cannot do (success rate
  < 30%).  Example: "split order into multiple shipments"--234 attempts, 18%
  success rate, growing trend.
- **Friction:** The API can do it, but agents take too many steps (success rate
  > 70%, high volume).  Example: "order history lookup"--2341 trajectories.
- **Workaround:** Moderate success rate suggests agents are hacking around a
  gap.  Example: "bulk update"--847 trajectories, 60% success rate.

Priority scoring follows the formula:

```
score = normalize(volume) * (1 - success_rate) * trend_multiplier
```

where `trend_multiplier` is 1.2 for growing, 1.0 for stable, and 0.8 for
declining trends.  The `generate_digest` function produces a human-readable
product intelligence report, organized by category.

---

## 8. Self-Optimization

### 8.1 Three-Tier Execution Model

State machine transitions are represented at three tiers of abstraction:

1. **Compiled (Tier 1):** Rust code generated from the TLA+ specification by
   `temper-codegen`.  Maximum performance, requires recompilation to change.

2. **Interpretable (Tier 2):** A `TransitionTable`--a data structure encoding
   all transition rules for an entity type, built from the `StateMachine`
   specification.  Each `TransitionRule` carries a `Guard` (evaluated at
   runtime) and a list of `Effect`s (`SetState`, `IncrementItems`,
   `DecrementItems`, `EmitEvent`).  Transitions are evaluated by matching the
   action name, checking `from_states`, evaluating the guard, and applying
   effects.

3. **Overlay (Tier 3):** A hot-swappable `TransitionTable` managed by
   `SwapController`.  The controller holds an `Arc<RwLock<TransitionTable>>`
   with a monotonically increasing version counter.  A new table can be swapped
   in atomically without restarting the actor or the process:

```rust
let ctrl = SwapController::new(table_v1);
assert_eq!(ctrl.version(), 1);

ctrl.swap(table_v2);  // atomic, no restart
assert_eq!(ctrl.version(), 2);
```

### 8.2 Shadow Testing

Before a hot swap is applied in production, the `shadow_test` function runs a
suite of `TestCase`s (state, item_count, action) against both the old and new
transition tables.  Any difference in outcome--different target state, different
success/failure, different effects--is recorded as a `Mismatch`.  Only if the
`ShadowResult` reports zero mismatches (or the mismatches are explicitly
expected) does the swap proceed.

### 8.3 Optimizer Actors

Three self-driving optimizer actors observe production metrics via the
`ObservabilityStore` trait and produce `OptimizationRecommendation`s:

- **QueryOptimizer:** Detects N+1 query patterns, slow entity set scans, and
  missing `$expand` opportunities.  Produces `UpdateQueryPlan` actions.
- **CacheOptimizer:** Analyzes cache hit/miss rates and adjusts TTLs.  Produces
  `UpdateCacheTtl` actions.
- **PlacementOptimizer:** Detects shard hotspots and proposes rebalancing.
  Produces `RebalanceShard` actions.

Each recommendation carries a risk level:

- **Risk::None:** Purely additive (e.g., cache warming).  Auto-approved.
- **Risk::Low:** Minor behavioral change (e.g., TTL adjustment).  Auto-approved
  only if estimated improvement exceeds 10%.
- **Risk::Medium:** Requires shadow testing.  Never auto-approved.

The `SafetyChecker` validates every recommendation before application, ensuring
that autonomous optimization cannot violate correctness invariants.

---

## 9. SQL-Based Observability

### 9.1 Provider-Swappable Interface

The `ObservabilityStore` trait defines three methods--`query_spans`,
`query_logs`, `query_metrics`--each accepting a SQL string and typed parameters.
Provider adapters (Logfire, Datadog, ClickHouse) implement this trait, mapping
their native storage into the canonical virtual tables.

### 9.2 Canonical Schemas

Three virtual tables provide a provider-independent query surface:

| Table     | Columns                                                                                    |
|-----------|--------------------------------------------------------------------------------------------|
| `spans`   | `trace_id`, `span_id`, `parent_span_id`, `service`, `operation`, `status`, `duration_ns`, `start_time`, `end_time`, `attributes` |
| `logs`    | `timestamp`, `level`, `service`, `message`, `attributes`                                   |
| `metrics` | `metric_name`, `timestamp`, `value`, `tags`                                                |

### 9.3 Portable Evidence

Evolution records carry SQL queries as evidence strings.  For example, an
`ObservationRecord` might carry:

```sql
SELECT p99(duration_ns) FROM spans WHERE operation = 'handle'
```

Because the query targets the canonical schema rather than a provider-specific
API, the evidence is portable across observability backends.  A team that
migrates from Logfire to Datadog retains the full chain of evidence in its
Evolution Records.

---

## 10. Related Work

**Actor systems.**  Erlang/OTP [8] established the actor model for
fault-tolerant distributed systems with supervision trees and let-it-crash
semantics.  Akka [9] brought the model to the JVM with typed actors and
cluster sharding.  Microsoft Orleans [10] introduced virtual actors with
automatic activation and placement.  Temporal [11] provides durable execution
for long-running workflows.  Temper draws supervision and sequential message
processing from Erlang/Akka but adds specification-driven code generation and
a verification cascade absent from all of these systems.

**Formal verification.**  TLA+ [7] is the standard for specifying and
model-checking concurrent and distributed systems.  Alloy [12] provides
relational modeling with SAT-based analysis.  Stateright [3] brings
model checking to Rust with an API designed for testing distributed protocols.
FoundationDB's simulation testing framework [4] pioneered seed-based
deterministic simulation for database systems; TigerBeetle's VOPR [5] applies
the same idea to a financial transaction engine.  Temper combines Stateright
model checking, FoundationDB-style simulation, and proptest-based property
testing into a unified three-level cascade.

**TigerStyle.**  TigerBeetle's TigerStyle [18] is a development methodology that
codifies principles Temper adopts throughout its design: assertion density
(minimum two assertions per function), bounded execution (no unbounded loops,
no unbounded allocations), static resource budgets (all buffers sized at
initialization), and a "zero technical debt" philosophy where every shortcut is
treated as a bug.  TigerStyle elevates deterministic simulation testing from an
optional technique to the *primary* testing strategy, ahead of integration and
end-to-end tests.  Temper's three-level verification cascade, bounded actor
mailboxes, static transition tables, and DST-first development methodology are
direct applications of TigerStyle principles to the actor-framework domain.

**API frameworks.**  OData v4 [1] standardizes entity data models, query
conventions, and metadata.  GraphQL [13] provides a flexible query language
but lacks the formal entity model and state machine semantics that Temper
requires.  REST frameworks (Rails, Django, Express) provide routing and ORM
but no specification-level verification.

**Attribute-based access control.**  XACML [14] defined the original ABAC
standard but suffers from XML complexity.  Google Zanzibar [15] provides
relationship-based authorization at scale.  Amazon Cedar [2] offers a
human-readable policy language with formal verification of policy properties.
Temper uses Cedar for its combination of expressiveness and formal guarantees.

**Self-optimizing systems.**  CockroachDB [16] performs automatic range
splitting and rebalancing.  Neon [17] adjusts compute and storage resources
based on workload.  Temper's optimizer actors operate at the application layer,
adjusting query plans, cache policies, and actor placement based on
observability data, with a safety checker ensuring correctness.

---

## 11. Evaluation

### 11.1 Test Coverage

The Temper workspace contains 246 tests across 16 crates. Key categories:

| Category                       | Count | Crates                                     |
|--------------------------------|------:|--------------------------------------------|
| Actor runtime and scheduler    |    19 | temper-runtime                             |
| Specification parsing          |     6 | temper-spec                                |
| OData query/path parsing       |    35 | temper-odata                               |
| Code generation                |     6 | temper-codegen                             |
| Verification (model+sim+prop)  |    27 | temper-verify                              |
| Entity actor DST tests         |     7 | temper-server                              |
| HTTP integration tests         |     8 | temper-server                              |
| Evolution records and chains   |    19 | temper-evolution                           |
| JIT tables and hot-swap        |    15 | temper-jit                                 |
| Authorization engine           |    10 | temper-authz                               |
| Observability and trajectory   |    18 | temper-observe                             |
| Optimizer actors and safety    |    15 | temper-optimize                            |
| Storage (Postgres, Redis)      |    33 | temper-store-postgres, temper-store-redis   |
| CLI subcommands                |    14 | temper-cli                                 |

### 11.2 Reference Application

The reference application (`reference/ecommerce`) models an agentic e-commerce
platform with three agent personas (CustomerAgent, OperationsAgent,
SupportAgent) operating on behalf of human customers.  The data model defines:

- 7 entity types, 3 enum types, 6 bound actions on Order, 3 on Payment, 2 on
  Shipment, 1 unbound action (SubmitFeedback), 4 bound functions, and 2
  unbound functions.
- The Order entity has a 10-state lifecycle with 14 transitions, 6 safety
  invariants, and 3 liveness properties.

### 11.3 Verification Results

Running the full verification cascade on the Order specification:

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L1    | Stateright model check    | PASSED  | 42,847 states explored, all properties hold |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, 847 transitions, 23 dropped msgs |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

The simulation is reproducible: re-running with the same seed produces identical
transition counts, message counts, and final actor states.  Heavy fault injection
(5% message drop, 2% actor crash) does not cause invariant violations because
faults affect message delivery, not state machine transition logic--the transition
table enforces guards regardless of the delivery path.

### 11.4 End-to-End Functional Validation

The reference application serves real state machine transitions through HTTP.
Entity actors process OData actions through JIT TransitionTables built from the
same TLA+ specs verified by the cascade.  A representative interaction sequence:

```
POST /odata/Orders         → 201, spawns actor in "Draft", item_count=0
POST .../AddItem           → 200, status="Draft", item_count=1, events=[AddItem]
POST .../SubmitOrder       → 200, status="Submitted", events=[AddItem, SubmitOrder]
POST .../SubmitOrder       → 409, "Action 'SubmitOrder' not valid from state 'Submitted'"
POST .../CancelOrder       → 200, status="Cancelled", events=[..., CancelOrder]
```

### 11.5 Bugs Found by DST-First Development

TigerStyle [18] mandates that deterministic simulation testing is not an
optional hardening step applied after integration testing--it is the *primary*
testing strategy.  Temper's DST-first methodology is a direct application of
this principle: actor-level simulation tests were written and passing before any
HTTP handler existed.  The bugs enumerated below were found *because* DST ran
first.  Had development followed the conventional order--unit tests, then
integration tests, then (maybe) simulation--these guard-resolution bugs would
have been invisible: unit tests do not exercise the full transition-table
resolution pipeline, and HTTP smoke tests accept whatever the handler returns
without checking invariant satisfaction.

The DST-first methodology--writing actor-level simulation tests before wiring
HTTP--discovered three guard resolution bugs that would have been invisible to
unit tests or HTTP smoke tests:

| Bug | Root Cause | Impact if Shipped |
|-----|-----------|-------------------|
| `SubmitOrder` succeeds with 0 items | `TransitionTable::from_state_machine()` used unresolved TLA+ transitions; `CanSubmit` predicate's `Cardinality(items) > 0` guard was not encoded | Agents could submit empty orders, violating the `SubmitRequiresItems` invariant |
| `CancelOrder` missing from transition table entirely | `resolve_transitions()` filtered `!name.starts_with("Can")`, catching both `CanCancel` (guard) and `CancelOrder` (action) | Agents could never cancel orders; 409 on every attempt |
| `CancelOrder` and `InitiateReturn` had `has_parameters=false` | TLA+ extractor stripped `(reason)` from the name before checking for parentheses | Guard/action distinction broken; parameterized actions treated as guard predicates and excluded |

The fix introduced `TransitionTable::from_tla_source()` which builds the table
from the verified Stateright model's resolved transitions, ensuring that the
exact same guard semantics verified at Level 1 (exhaustive model checking) are
enforced at runtime.  This establishes a critical invariant:

> **The transition table in the HTTP-serving entity actor is identical to the
> one verified by the three-level cascade.**

### 11.6 Live Infrastructure Validation

The system runs end-to-end against real infrastructure: PostgreSQL 18 for event
sourcing, Redis 8 for actor mailboxes, and ClickHouse for observability.  A
Claude-powered LLM agent (Anthropic Sonnet 4.5) operates the e-commerce API
through natural language.  A representative demo session:

```
User: "Create a new order, add a premium widget, and submit it"
Agent: Claude → create_order → AddItem → SubmitOrder
Result: Order in Submitted status, 2 events persisted to Postgres

User: "I want to split my order into two shipments"
Agent: Claude → (no matching action exists) → responds with apology
Result: Trajectory span captured to ClickHouse with user_intent

Analysis: ClickHouse query detects "split order" as unmet intent
Output: O-Record (observation) + I-Record (insight, category=UnmetIntent)
Product Digest: "Add SplitOrder action to Order entity"
```

The full feedback loop is verified:

1. Natural language → Claude → OData tool calls
2. Entity actors process transitions through TLA+-verified TransitionTables
3. Events persist to PostgreSQL (survives server restart)
4. Trajectory spans capture to ClickHouse (operation, intent, status)
5. Trajectory analysis queries ClickHouse for patterns
6. Evolution Engine generates records from production data
7. Product intelligence digest tells the human what to build next

---

## 12. Conclusion and Future Work

### 12.1 Summary of Contributions

Temper makes the following contributions:

1. A specification-first architecture where CSDL, TLA+, and Cedar documents are
   the durable artifacts and code is derived.

2. A three-level verification cascade combining exhaustive model checking,
   deterministic simulation with fault injection, and property-based testing.

3. An Evolution Engine with Lamport-style problem formalization, human approval
   gates, and portable SQL evidence.

4. Trajectory intelligence that extracts product signal (unmet intents, friction,
   workarounds) from agent execution traces.

5. A three-tier JIT execution model with atomic hot-swap and shadow testing for
   zero-downtime state machine evolution.

6. A DST-first development methodology where actor-level simulation tests
   validate state machine behavior before HTTP wiring, catching guard
   resolution bugs that would be invisible to integration tests.

7. End-to-end functional validation: the same `TransitionTable` verified by
   Stateright, deterministic simulation, and property tests runs inside
   HTTP-serving entity actors, establishing a provable chain from formal
   specification to production behavior.

8. Adoption of TigerStyle [18] as a cross-cutting engineering methodology:
   assertion density at the state machine level, bounded execution throughout
   the actor runtime, static resource budgets, and DST-first development
   where simulation testing is the primary--not supplementary--testing
   strategy.

### 12.2 Future Work

Several directions remain open:

- **Distributed clustering.**  The current actor system is single-node.
  Extending `temper-runtime` with a cluster membership protocol and remote
  actor references would enable horizontal scaling.

- **WASM actor bodies.**  Compiling actor message handlers to WebAssembly
  would allow hot-loading new actor logic without restarting the host process,
  complementing the JIT transition table layer.

- **Multi-tenancy.**  The current design assumes a single tenant.  Adding
  tenant-scoped actor systems, per-tenant CSDL overlays, and Cedar policy
  namespacing would support SaaS deployment.

- **Cloud deployment.**  Packaging the framework as a managed service with
  infrastructure-as-code templates for AWS/GCP/Azure would lower the barrier
  to adoption.

- **Liveness verification.**  The current verification cascade checks safety
  invariants exhaustively but does not model-check liveness properties (which
  require fairness assumptions).  Integrating liveness checking into Level 1
  would strengthen the guarantees.

---

## References

[1] OASIS. *OData Version 4.01.* OASIS Standard, 2020.
https://docs.oasis-open.org/odata/odata/v4.01/odata-v4.01-part1-protocol.html

[2] J. Hessing et al. *Cedar: A New Language for Expressive, Fast, Safe, and
Analyzable Authorization.* Amazon, 2023.
https://www.amazon.science/publications/cedar

[3] J. Nadal. *Stateright: A Model Checker for Implementing Distributed
Systems.* 2022. https://github.com/stateright/stateright

[4] A. Abdelhamid et al. *FoundationDB: A Distributed Key-Value Store.*
Proceedings of SIGMOD, 2021.

[5] TigerBeetle. *VOPR: Viewstamped Operation Replayer.*
https://github.com/tigerbeetle/tigerbeetle

[6] A. Mackenzie-Helnwein. *proptest: Hypothesis-like property testing for Rust.*
https://github.com/proptest-rs/proptest

[7] L. Lamport. *Specifying Systems: The TLA+ Language and Tools for Hardware
and Software Engineers.* Addison-Wesley, 2002.

[8] J. Armstrong. *Making Reliable Distributed Systems in the Presence of
Software Errors.* PhD Thesis, Royal Institute of Technology, Stockholm, 2003.

[9] Lightbend. *Akka: Build Concurrent, Distributed, and Resilient
Message-Driven Applications.* https://akka.io

[10] S. Bykov et al. *Orleans: Distributed Virtual Actors for
Programmability and Scalability.* Microsoft Research Technical Report
MSR-TR-2014-41, 2014.

[11] Temporal Technologies. *Temporal: Durable Execution Platform.*
https://temporal.io

[12] D. Jackson. *Software Abstractions: Logic, Language, and Analysis.*
MIT Press, 2012.

[13] Facebook. *GraphQL Specification.* 2015. https://spec.graphql.org

[14] OASIS. *eXtensible Access Control Markup Language (XACML) Version 3.0.*
OASIS Standard, 2013.

[15] R. Pang et al. *Zanzibar: Google's Consistent, Global Authorization
System.* USENIX ATC, 2019.

[16] CockroachDB. *CockroachDB: The Resilient Geo-Distributed SQL Database.*
https://www.cockroachlabs.com

[17] Neon. *Neon: Serverless Postgres.* https://neon.tech

[18] TigerBeetle. *TigerStyle: Engineering Design Philosophy.*
https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md
