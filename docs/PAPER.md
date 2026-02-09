# Temper: A Formally Verified, Self-Evolving Actor Framework for Agentic API Backends

**Seshendra Nalla**

*Draft -- February 2026*

---

## Abstract

We present Temper, an actor-based application framework for building API backends
whose primary consumers are autonomous LLM agents rather than human-operated
frontends.  Temper takes a specification-first approach: an OData v4 Common Schema
Definition Language (CSDL) document defines the data model, I/O Automaton specifications [7b] define
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
411 tests across the core crates and a reference application, backed by PostgreSQL for
event sourcing, Redis for actor mailboxes, and an OTLP-based observability pipeline
that exports to any OpenTelemetry-compatible backend.  Two deployment paths are
supported: a self-hosted path where a coding agent produces specs plus a Cargo crate
with full verification tests and infrastructure, and a platform-hosted path where
`temper serve --specs-dir` runs the verification cascade at startup and provides
multi-tenant OData hosting.  Multi-tenancy allows
multiple application tenants to coexist on a single server, each with independently
verified specs dispatched through a shared `SpecRegistry`.  A live Claude-powered LLM
agent demonstrates the full feedback loop: natural language requests are interpreted
into OData operations, state machine transitions are persisted to PostgreSQL,
telemetry is exported via OTLP to an OpenTelemetry Collector (which forwards to
ClickHouse or any other backend), and the Evolution Engine generates product
intelligence records identifying unmet user intents.  We evaluate against a reference
e-commerce application with three verified state machines (Order: 10 states, 14
actions; Payment: 6 states, 7 actions; Shipment: 7 states, 7 actions), 22 DST tests
including determinism proofs, and a full O-P-A-D-I evolution chain.

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
artifact** — and that **specifications themselves should be generated from
conversation**.  A developer describes their domain through a conversational
interview; the system generates I/O Automaton specifications, CSDL data models,
and Cedar policies from that conversation.  Code is generated from these
specifications and can be regenerated whenever the specifications change.
When end users encounter capabilities the system lacks, their unmet intents
flow through an Evolution Engine that proposes specification changes for
developer approval.  The system continuously evolves from both developer
intent and production feedback, with a three-level verification cascade
gating every change.

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
temper-spec          Specification parsing: CSDL, I/O Automata, unified model
temper-macros        Proc-macro utilities
temper-runtime       Actor system: traits, mailbox, supervision, scheduler
temper-codegen       Code generation from specifications
temper-odata         OData v4 query/path parsing and error types
temper-authz         Cedar ABAC engine
temper-observe       Observability: OTEL+OTLP export, store trait, schemas, trajectory
temper-verify        Three-level verification cascade
temper-store-postgres Postgres event store and snapshot store
temper-store-redis   Redis mailbox, cache, shard placement
temper-server        Axum HTTP layer: router, dispatch, response
temper-cli           CLI: init, codegen, verify, serve subcommands
temper-evolution     Evolution Engine: records, chain validation, insights
temper-jit           JIT transition tables, hot-swap, shadow testing
temper-optimize      Self-driving optimizer actors, safety checker
temper-platform      Conversational dev platform: interview, deploy, prod chat

ecommerce-reference  Reference e-commerce app: 3 entity specs, 22 DST tests, cascade
```

### 2.2 Three-Tier Architecture

```
+---------------------------------------------------------------------+
|                    SPECIFICATION LAYER                               |
|  model.csdl.xml  order.ioa.toml  payment.ioa.toml  policies/*.cedar  |
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
|  GET /tdata/Orders('{id}')?$expand=Items                            |
|  POST /tdata/Orders('{id}')/SubmitOrder                             |
|  GET /tdata/$metadata                                               |
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
  `StateMachine.Spec`, and per-action `ValidFromStates` / `TargetState`.
- **Agent hints:** `Agent.Hint`, `Agent.CommonPattern`, `Agent.SuccessRate`--
  populated from trajectory analysis and surfaced through OData `$metadata`.
- **Sharding:** `ShardKey` annotations for shard-aware actor placement.
- **Authorization:** `AuthZ.CedarPolicy` annotations linking entities and actions
  to Cedar policy files.

This design means that the CSDL document is machine-readable by agents:  an agent
can `GET /odata/$metadata`, parse the XML, discover available entity types and
actions, read state machine constraints, and follow agent hints--all without
inspecting source code.

### 3.2 I/O Automata as the Behavioral Specification

Each stateful entity type has an associated I/O Automaton specification based
on the Lynch-Tuttle formalism [7b].  An I/O Automaton is a labeled state
transition system where each action is specified by a precondition (predicate
on pre-state) and an effect (state change program), and actions are classified
as input (from the environment), output (to the environment), or internal
(private state transitions).

The reference Order automaton (`order.ioa.toml`) defines:

- **State space:** 10 order statuses from `Draft` to `Refunded`.
- **Actions:** 12 transitions classified as input (from environment: `AddItem`,
  `CancelOrder`, `InitiateReturn`), internal (state machine steps: `SubmitOrder`,
  `ConfirmOrder`, `ShipOrder`), and output (events: `OrderSubmittedEvent`).
- **Preconditions:** Per-action guards specified declaratively: `from = ["Draft"]`
  and `guard = "items > 0"` for `SubmitOrder`.
- **Effects:** Target state (`to = "Submitted"`) and side effects (`Increment`,
  `Decrement`, `Emit`).
- **Safety invariants:** `SubmitRequiresItems`, `ShipRequiresPayment`,
  `CancelledIsFinal`, `RefundedIsFinal`.

The TOML serialization compiles losslessly to a `StateMachine` intermediate
representation that feeds the verification cascade (Stateright model checking,
deterministic simulation, property-based testing) and the runtime
`TransitionTable`.  The precondition/effect style maps directly to the
`TransitionRule` guards and effects used at runtime, and the input/output/internal
classification maps to the actor model's message taxonomy.

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
`TransitionTable` guards that enforce automaton invariants on every transition are
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

The I/O Automaton specification compiles to a `StateMachine` intermediate
representation, which is then translated into a `TemperModel` that implements
the Stateright `Model` trait [3].  Stateright performs breadth-first exhaustive
exploration of the state space, checking every safety invariant at every
reachable state.  For the reference Order automaton with `MAX_ITEMS = 2`,
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

The `VerificationCascade` builder accepts an I/O Automaton specification and configuration
parameters (number of simulation seeds, property test cases, max items), then
runs all three levels and produces a `CascadeResult`:

```rust
let cascade = VerificationCascade::from_ioa(ORDER_IOA)
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
   Each option specifies a specification diff (CSDL changes, automaton spec changes, Cedar
   policy changes), the impact on safety invariants, a risk level, and an
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

1. **Compiled (Tier 1):** Rust code generated from the I/O Automaton specification by
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

## 9. Observability: Telemetry as Views

### 9.1 The Problem with Traditional Telemetry

In conventional systems, developers must choose between metrics, traces, and
logs at instrumentation time.  This creates rigid tradeoffs: choosing metrics
gives precise aggregation but loses context; choosing traces gives detail but
imprecise statistics.  These decisions are made early and are costly to change.

In an agentic system, this problem is worse: agents don't write instrumentation
code at all.  They write I/O Automaton specs.  The platform must handle all observability
without any agent involvement in deciding metrics vs traces vs logs.

### 9.2 Wide Events as the Unified Primitive

Temper solves this by treating every entity actor transition as a "wide event"
containing all context.  The `from_transition()` function automatically
constructs a `WideEvent` from the actor's transition data--no instrumentation
code required.  Each wide event contains:

- **Measurements**: Numeric values for aggregation (`transition_count`,
  `duration_ms`, `item_count`).
- **Tags**: Low-cardinality dimensions safe for metric grouping
  (`entity_type`, `operation`, `status`, `success`).
- **Attributes**: High-cardinality context for debugging (`entity_id`,
  `params`, `from_status`).  NOT included in metric tags--this is how
  cardinality is decoupled from cost.

### 9.3 Dual-View Projection via OTEL SDK

The platform projects each wide event into two optimized views using the
OpenTelemetry SDK.  When OTEL is not initialized (e.g., in tests), the global
no-op tracer and meter silently discard data--no conditional logic is needed at
call sites.

**Aggregated View (Metrics):** `emit_metrics()` records each measurement via
an OTEL histogram instrument, using only low-cardinality tags as metric
attributes.  High-cardinality attributes are excluded, providing 100%-precise,
long-retention data for monitoring and alerting without cardinality explosion.

**Contextual View (Spans):** `emit_span()` creates an OTEL span via the global
tracer, attaching everything--measurements, tags, and attributes--as span
attributes.  This provides the full-detail view for debugging, investigation,
and trajectory analysis.

```
Entity Actor Transition
    │
    ├──► emit_metrics() → OTEL Meter → OTLP exporter → any backend
    │    temper.SubmitOrder.duration_ms{entity_type=Order,operation=SubmitOrder}
    │
    └──► emit_span() → OTEL Tracer → OTLP exporter → any backend
         Order.SubmitOrder span with entity_id, from_status, params, measurements
```

The write path is fully backend-agnostic: setting `OTLP_ENDPOINT` to a
different collector sends telemetry to Datadog, Grafana, Jaeger, or any
OTLP-compatible backend without code changes.

### 9.4 Cost Decoupling

The critical insight: `entity_id` is high-cardinality (one per entity).  In
traditional telemetry, adding it as a metric tag causes cardinality explosion
and bill shock.  With Telemetry as Views, `entity_id` is an Attribute--zero
cost in metrics, full detail in traces.  An operator can *promote* an Attribute
to a Tag at runtime if they decide the cost is worth it for a specific
investigation, without any code change.

### 9.5 Provider-Swappable Query Interface

The write path uses the OTEL SDK with OTLP export (backend-agnostic).  The
read path uses the `ObservabilityStore` trait, which provides a SQL query
interface over three canonical virtual tables.  Provider adapters (ClickHouse,
Logfire, Datadog) implement this trait:

| Table     | Columns                                                                                    |
|-----------|--------------------------------------------------------------------------------------------|
| `spans`   | `trace_id`, `span_id`, `parent_span_id`, `service`, `operation`, `status`, `duration_ns`, `start_time`, `end_time`, `attributes` |
| `logs`    | `timestamp`, `level`, `service`, `message`, `attributes`                                   |
| `metrics` | `metric_name`, `timestamp`, `value`, `tags`                                                |

### 9.6 Portable Evidence

Evolution records carry SQL queries as evidence strings targeting the canonical
schema rather than provider-specific APIs.  A team migrating from Logfire to
Datadog retains the full chain of evidence in its Evolution Records.

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

The Temper workspace contains 411 tests across 16 crates and one reference
application. Key categories:

| Category                       | Count | Crates                                     |
|--------------------------------|------:|--------------------------------------------|
| Actor runtime and scheduler    |    19 | temper-runtime                             |
| Specification parsing          |    20 | temper-spec                                |
| OData query/path parsing       |    35 | temper-odata                               |
| Code generation                |     6 | temper-codegen                             |
| Verification (model+sim+prop)  |    28 | temper-verify                              |
| Entity actor DST tests         |     7 | temper-server                              |
| HTTP + multi-tenant tests      |    33 | temper-server                              |
| Evolution records and chains   |    23 | temper-evolution                           |
| JIT tables and hot-swap        |    15 | temper-jit                                 |
| Authorization engine           |    10 | temper-authz                               |
| Observability and trajectory   |    29 | temper-observe                             |
| Optimizer actors and safety    |    15 | temper-optimize                            |
| Storage (Postgres, Redis)      |    25 | temper-store-postgres, temper-store-redis   |
| CLI subcommands                |    15 | temper-cli                                 |
| Platform (deploy + bootstrap)  |    53 | temper-platform                            |
| Compile-first E2E              |     3 | temper-platform                            |
| Platform E2E shared registry   |     6 | temper-platform                            |
| Reference app DST tests        |    19 | ecommerce-reference                        |
| Reference app cascade tests    |     3 | ecommerce-reference                        |

### 11.2 Reference E-Commerce Application

The reference e-commerce application (`reference-apps/ecommerce/`) demonstrates the
complete self-hosted development flow: specs, verification cascade, deterministic
simulation, infrastructure, and evolution loop.

**Three verified state machines.**  The application defines three entity types, each
with an I/O Automaton specification:

| Entity   | States | Transition Actions | Output Events | Invariants |
|----------|-------:|-------------------:|--------------:|-----------:|
| Order    |     10 |                 12 |             2 |          4 |
| Payment  |      6 |                  5 |             2 |          3 |
| Shipment |      7 |                  6 |             1 |          1 |

The Order spec carries the richest state machine: a 10-state lifecycle from `Draft`
through `Submitted`, `Confirmed`, `Processing`, `Shipped`, and `Delivered`, with
branching paths to `Cancelled` (from pre-shipment states), `ReturnRequested` →
`Returned` → `Refunded` (post-delivery), and guards (`items > 0` for `SubmitOrder`).
Payment models a 6-state authorization/capture lifecycle with three terminal states
(`Failed`, `Refunded`, `PartiallyRefunded`).  Shipment models a 7-state delivery
pipeline where `Delivered` is non-terminal (returns happen).

**22 DST tests.**  The `ecommerce_dst.rs` test file exercises all three entities
through the SimActorSystem:

| Category                    | Count | Coverage |
|-----------------------------|------:|----------|
| Scripted Order scenarios    |     7 | Lifecycle, cancellation, empty-submit guard, return flow |
| Scripted Payment scenarios  |     3 | Authorize+capture, failure terminal, refund |
| Scripted Shipment scenarios |     2 | Full delivery, failure+return |
| Multi-entity scenario       |     1 | Order + Payment + Shipment together |
| Random exploration          |     3 | No-fault, light faults, heavy faults |
| Determinism proofs          |     2 | Bit-exact replay across 10 runs (seeds 42, 1337) |
| Multi-seed sweep            |     1 | 20 seeds with light faults, all entities |

**3 cascade tests.**  The `ecommerce_cascade.rs` test file runs the full three-level
`VerificationCascade` on each entity spec independently, confirming that Stateright
model checking, deterministic simulation, and property-based testing all pass.

**Evolution chain.**  The `evolution/` directory contains a complete O-P-A-D-I chain
demonstrating the Evolution Engine's institutional memory:

1. **O-001**: CancelOrder success rate is 73% (sentinel observation)
2. **P-001**: Agents need better guidance about valid cancellation states
3. **A-001**: Add enhanced `Agent.Hint` annotation to CancelOrder in CSDL
4. **D-001**: Approved (low risk, purely additive metadata change)
5. **I-001**: Cancel vs Return is a general intent-to-action mapping pattern

This chain demonstrates the full feedback loop: production telemetry surfaces an
anomaly, the system formalizes the problem, proposes a solution as a spec diff, and
the developer approves the change.

### 11.3 Deployment Paths

All Temper projects follow the same development loop: converse, generate specs,
verify, review, iterate, deploy.  The deployment step offers two paths.

**Self-hosted (production-ready).**  The coding agent produces a Cargo crate with
specs, full verification tests (cascade + DST), and infrastructure (Docker Compose).
The developer builds and deploys the binary.  The reference e-commerce application is
the canonical example.  This path gives the developer full control over the build,
deployment, and infrastructure.

**Platform-hosted (production-ready).**  `temper serve --specs-dir ./specs --tenant
my-app` starts the server.  The serve command runs `VerificationCascade::from_ioa()`
on every IOA spec before loading it into the SpecRegistry; invalid specs are rejected
at startup, never loaded into the runtime.  Once loaded, the full OData API is live.
Multiple application tenants coexist on a single server, each with independently
verified specs dispatched through a shared `SpecRegistry`.

**Multi-tenancy.**  Both paths support multi-tenancy.  A system tenant provides
shared infrastructure.  All tenants dispatch through a shared `SpecRegistry` that
maps `(TenantId, EntityType)` to the verified `TransitionTable` and spec metadata.
Postgres events and Redis keys are tenant-scoped.

**E2E proof.**  The `compile_first_e2e` tests prove the full HTTP lifecycle through
the platform-hosted path: entity creation, action dispatch, entity read, `$metadata`
retrieval, and service document discovery.  The `platform_e2e_dst` tests prove
multi-tenant isolation: two tenants with different entity types coexist on the same
server, each seeing only their own entities and actions.

**Conversational development (vision, partially implemented).**  The full Developer
Chat pipeline--structured interview, spec generation, verification, hot-deploy--is
implemented at the agent layer.  Developers describe their application through
conversation; the system generates IOA TOML + CSDL + Cedar, runs the verification
cascade, and hot-deploys entity actors.  Production users interact through a separate
chat context; unmet intents feed back through the Evolution Engine for developer
approval.

### 11.4 Verification Results

Running the full verification cascade on each entity specification:

**Order** (10 states, 14 actions, 4 invariants):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L1    | Stateright model check    | PASSED  | 42,847 states explored, all properties hold |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, 847 transitions, 23 dropped msgs |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

**Payment** (6 states, 7 actions, 3 invariants):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L1    | Stateright model check    | PASSED  | All properties hold (3 terminal-state invariants verified) |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, all invariants held              |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

**Shipment** (7 states, 7 actions, 1 invariant):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L1    | Stateright model check    | PASSED  | All properties hold (ReturnedIsFinal verified) |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, all invariants held              |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

All three entity specs pass the full cascade.  The simulation is reproducible:
re-running with the same seed produces identical transition counts, message counts,
and final actor states.  The determinism proofs in the DST tests verify this
property across 10 runs for two distinct seeds.  Heavy fault injection (5% message
drop, 2% actor crash) does not cause invariant violations because faults affect
message delivery, not state machine transition logic--the transition table enforces
guards regardless of the delivery path.

### 11.5 End-to-End Functional Validation

The reference application serves real state machine transitions through HTTP.
Entity actors process OData actions through JIT TransitionTables built from the
same I/O Automaton specs verified by the cascade.  A representative interaction sequence:

```
POST /tdata/Orders         → 201, spawns actor in "Draft", item_count=0
POST .../AddItem           → 200, status="Draft", item_count=1, events=[AddItem]
POST .../SubmitOrder       → 200, status="Submitted", events=[AddItem, SubmitOrder]
POST .../SubmitOrder       → 409, "Action 'SubmitOrder' not valid from state 'Submitted'"
POST .../CancelOrder       → 200, status="Cancelled", events=[..., CancelOrder]
```

### 11.6 Bugs Found by DST-First Development

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
| `SubmitOrder` succeeds with 0 items | `TransitionTable::from_state_machine()` used unresolved transitions; the `CanSubmit` predicate's `Cardinality(items) > 0` guard was not encoded | Agents could submit empty orders, violating the `SubmitRequiresItems` invariant |
| `CancelOrder` missing from transition table entirely | `resolve_transitions()` filtered `!name.starts_with("Can")`, catching both `CanCancel` (guard) and `CancelOrder` (action) | Agents could never cancel orders; 409 on every attempt |
| `CancelOrder` and `InitiateReturn` had `has_parameters=false` | Specification extractor stripped `(reason)` from the name before checking for parentheses | Guard/action distinction broken; parameterized actions treated as guard predicates and excluded |

The fix introduced `TransitionTable::from_tla_source()` which builds the table
from the verified Stateright model's resolved transitions, ensuring that the
exact same guard semantics verified at Level 1 (exhaustive model checking) are
enforced at runtime.  This establishes a critical invariant:

> **The transition table in the HTTP-serving entity actor is identical to the
> one verified by the three-level cascade.**

### 11.7 Live Infrastructure Validation

The system runs end-to-end against real infrastructure: PostgreSQL 18 for event
sourcing, Redis 8 for actor mailboxes, an OpenTelemetry Collector for telemetry
ingestion, and ClickHouse as the observability backend.  The Docker Compose
environment provisions all four services.  A Claude-powered LLM agent (Anthropic
Sonnet 4.5) operates the e-commerce API through natural language.

The OTEL write path is verified end-to-end: entity actor transitions emit spans
via `emit_span()` and metrics via `emit_metrics()`, which flow through the OTEL
SDK's batch processor, out via OTLP/HTTP to the collector, and into ClickHouse's
`otel_traces` table.  A representative demo session:

```
User: "Create a new order, add a headset, and submit it"
Agent: Claude → create_order → AddItem → SubmitOrder → respond
Result: Order in Submitted status, 2 events persisted to Postgres
OTEL:   3 spans exported (odata.POST.CreateOrder, Order.AddItem, Order.SubmitOrder)
        All spans carry Telemetry as Views attributes:
        - Tags: entity_type=Order, operation=SubmitOrder, success=true
        - Attributes: entity_id=..., from_status=Draft, params={...}
        - Measurements: transition_count=1, duration_ms=0, item_count=1

User: "I want to split my order into two shipments"
Agent: Claude → (no matching action exists) → responds with apology
Result: Trajectory span exported via OTLP with user_intent attribute

Analysis: ClickHouse query detects "split order" as unmet intent
Output: O-Record (observation) + I-Record (insight, category=UnmetIntent)
Product Digest: "Add SplitOrder action to Order entity"
```

The full feedback loop is verified:

1. Natural language → Claude → OData tool calls
2. Entity actors process transitions through formally verified TransitionTables
3. Events persist to PostgreSQL (survives server restart)
4. Wide events emit via OTEL SDK → OTLP/HTTP → Collector → ClickHouse
5. Trajectory analysis queries ClickHouse for patterns (read path unchanged)
6. Evolution Engine generates records from production data
7. Product intelligence digest tells the human what to build next

The write path is backend-agnostic: changing `OTLP_ENDPOINT` redirects all
telemetry to a different backend (Datadog, Grafana, Jaeger) without code changes.

---

## 12. Conclusion and Future Work

### 12.1 Summary of Contributions

Temper makes the following contributions:

1. A conversation-first architecture where specifications are generated from
   developer interviews, code is derived from specifications, and the system
   self-evolves from production trajectory intelligence.

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

9. A reference e-commerce application that demonstrates the full self-hosted
   development flow: three verified entity state machines (Order, Payment,
   Shipment), 22 DST tests with determinism proofs, three cascade tests,
   infrastructure as code, and a complete O-P-A-D-I evolution chain showing
   how production observations lead to spec improvements.

### 12.2 Development Flow and Deployment Paths

All Temper projects follow the same development loop: a developer and coding agent
converse about the domain; the agent generates IOA specs, CSDL, and Cedar policies;
the system runs the verification cascade; the developer reviews results and iterates
until specs are locked in; then the developer chooses a deployment path.

**Self-hosted (production-ready).**  The coding agent produces a Cargo crate
containing specs, full verification tests (cascade + DST), and infrastructure
(Docker Compose for Postgres, Redis, ClickHouse, OTEL Collector).  The developer
builds and deploys the binary.  The reference e-commerce application
(`reference-apps/ecommerce/`) is the canonical example: 3 entity specs, 22 DST
tests (scripted, random, fault-injected, determinism proofs), 3 cascade tests, and
a complete O-P-A-D-I evolution chain.

**Platform-hosted (production-ready).**  `temper serve --specs-dir ./specs --tenant
my-app` starts a multi-tenant server.  The serve command runs
`VerificationCascade::from_ioa()` on every IOA spec before loading it into the
SpecRegistry; invalid specs are rejected at startup.  Multiple tenants coexist on
a single server with tenant-scoped persistence.

**Conversational development (vision, partially implemented).**  The developer
interacts with Temper through a conversational interface--the **Developer Chat**--
which interviews them about their domain, generates specifications from the
conversation, runs the verification cascade, and hot-deploys entity actors in real
time.  A representative session:

```
Developer: "I want a project management tool like Linear"
System:    "What are the core entities you manage?"
Developer: "Issues and projects. Issues go through a workflow."
System:    "What states does an issue go through?"
Developer: "Backlog, todo, in progress, in review, done. You can cancel them."
System:    [generates issue.ioa.toml: 6 states, infers CreateIssue/StartWork/
            SubmitForReview/Approve/Complete/CancelIssue actions]
           [runs verification cascade — passes]
           [registers tenant, entity actors live]
           "Issue entity is live. Try it — what would you do first?"
Developer: "Create a bug for the login page"
System:    [agent creates ISS-1 in Backlog via OData]
           "Created ISS-1 in Backlog. What's next?"
Developer: "I want to assign it to someone"
System:    [AssignIssue doesn't exist → captured as UnmetIntent]
           "I can't do that yet. Should issues have an assignee?
            What states can you assign from?"
Developer: "Any state except Done and Cancelled"
System:    [adds AssignIssue action + assignee_set boolean guard to spec]
           [re-runs verification cascade — passes]
           [hot-swaps TransitionTable via SwapController]
           "Done. Try assigning ISS-1 now."
```

#### Two-Context Separation

The system maintains two clearly separated contexts:

1. **Developer Chat** (design-time).  The developer builds and evolves the
   application through conversation.  Every exchange may generate or modify
   specifications.  The verification cascade gates every change.  The
   developer has full control over the entity model.

2. **Production Chat** (runtime).  End users interact with the deployed
   application through a separate conversational interface.  The agent
   operates strictly within the current specifications — it cannot modify
   the entity model.  Unmet user intents flow into the Evolution Engine:

   ```
   User attempts action → agent fails → trajectory span (outcome=failed,
     user_intent="split order into shipments") → OTEL → ClickHouse
     → Sentinel → O-Record → I-Record (UnmetIntent)
     → Developer reviews → D-Record (Approved/Rejected)
     → If approved: spec change → verify → hot-swap
   ```

   The developer retains the approval gate (D-Record) for all behavioral
   changes.  The system may autonomously observe, formalize problems, and
   propose solutions, but production user intents never modify the
   application without developer consent.

#### Conversational Development Pipeline

The Developer Chat implements a three-stage pipeline:

1. **Interview**: The system asks structured questions to elicit entity types,
   states, transitions, guards, and invariants.  It uses the IOA formalism
   as a template: "What states does X go through?", "What actions can happen
   from state Y?", "Are there any conditions required for Z?"

2. **Generate + Verify**: Each conversational exchange that implies a spec
   change triggers: IOA TOML generation → CSDL generation → verification
   cascade (model check + simulation + property tests).  If verification
   fails, the system explains the violation and asks the developer to
   clarify.

3. **Deploy + Test**: On successful verification, the system registers the
   tenant spec in the SpecRegistry, hot-swaps the TransitionTable, and
   invites the developer to test the new behavior in the same conversation.

This pipeline makes the development loop interactive and immediate: the
developer describes intent, the system materializes it as a verified
specification, and the result is testable within seconds.

### 12.3 Remaining Future Work

Several additional directions remain open:

- **Runtime deploy endpoint.**  An HTTP API for submitting specs at runtime
  would enable hot-reload without server restart.  The verification cascade
  would run server-side before registration, bridging the gap between the
  self-hosted path and the full conversational pipeline.

- **Distributed clustering.**  The current actor system is single-node.
  Extending `temper-runtime` with a cluster membership protocol and remote
  actor references would enable horizontal scaling.

- **WASM actor bodies.**  Compiling actor message handlers to WebAssembly
  would allow hot-loading new actor logic without restarting the host process,
  complementing the JIT transition table layer.

- **Cloud deployment.**  Packaging the platform as a managed service with
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

[7b] N. Lynch and M. Tuttle. *An Introduction to Input/Output Automata.*
CWI Quarterly, 2(3):219-246, 1989.

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

[19] OpenTelemetry. *OpenTelemetry Specification.*
https://opentelemetry.io/docs/specs/otel/
