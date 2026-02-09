# Temper: What If Agents Could Converse Their Way to a Verified Backend?

**Seshendra Nalla**

*Draft -- February 2026*

---

## Abstract

Most enterprise SaaS applications follow a remarkably similar pattern: entities
move through state machines, guards prevent invalid transitions, integrations
notify external systems, and authorization policies control who can do what.
An e-commerce order, a support ticket, a subscription, an approval workflow --
the business logic in each case is a set of states, transitions, and invariants.
The rest is infrastructure.

This observation suggests that if the state machine is the essential artifact,
then much of the surrounding code -- controllers, service layers, ORM mappings,
webhook plumbing, instrumentation -- might be derivable rather than written.
And if specifications can be generated from conversation rather than hand-coded,
the feedback loop between what a user needs and what the system provides
tightens considerably.

We explore this idea through Temper, an actor-based framework where I/O Automaton
specifications define behavioral state machines, a four-level verification cascade
(SMT symbolic checking, exhaustive model checking, deterministic simulation, and
property-based testing) establishes correctness before deployment, and an evolution
engine captures unmet user intents from production to propose specification changes
back to the developer.  The state machine stays pure and deterministically verifiable;
external integrations follow an outbox pattern, dispatched asynchronously from the
event journal.  A self-describing HTTP API is derived automatically from the data
model, giving agents a structured interface they can navigate without documentation.
A pre-built rule index yields sub-30ns action evaluation, while end-to-end benchmarks
through the full HTTP stack with PostgreSQL persistence show ~18ms per action and
~2,200 persisted actions per second under concurrent load.

The framework is implemented as a 16-crate Rust workspace with 440+ tests and a
reference e-commerce application (three entity types, seven verified specifications
across different SaaS domains).  We do not claim this approach generalizes to all
backend systems -- but for the substantial class of applications whose core logic
is state machine shaped, the results suggest that specification-first, conversation-
driven development is a practical path worth investigating further.

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
interview; the system generates I/O Automaton behavioral specifications, OData
CSDL data models, and Cedar authorization policies from that conversation.
Code is generated from these specifications and can be regenerated whenever the
specifications change.
When end users encounter capabilities the system lacks, their unmet intents
flow through an Evolution Engine that proposes specification changes for
developer approval.  The system continuously evolves from both developer
intent and production feedback, with a four-level verification cascade
gating every change.

The remainder of this paper is organized as follows.  Section 2 presents the
overall architecture.  Sections 3--5 describe the specification layer, the actor
runtime, and the verification cascade.  Sections 6--8 cover the Evolution Engine,
trajectory intelligence, and self-optimization.  Section 9 describes the
observability subsystem.  Section 10 surveys related work.  Section 11 evaluates
the framework against the reference application.  Section 12 concludes.

---

## 2. Architecture Overview

The system is organized into four layers, each with a distinct role in the
lifecycle of a specification:

```
+---------------------------------------------------------------------+
|                    SPECIFICATION LAYER                               |
|  model.csdl.xml  order.ioa.toml  payment.ioa.toml  policies/*.cedar  |
+---------------------------------------------------------------------+
          |                  |                  |
          v                  v                  v
+---------------------------------------------------------------------+
|                      ACTOR RUNTIME                                  |
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
|                                                                     |
|  GET /tdata/Orders('{id}')?$expand=Items                            |
|  POST /tdata/Orders('{id}')/SubmitOrder                             |
|  GET /tdata/$metadata                                               |
+---------------------------------------------------------------------+
          |                  |                  |
          v                  v                  v
+---------------------------------------------------------------------+
|                   FEEDBACK LOOP                                     |
|                                                                     |
|  Sentinel ──> O-Record ──> P-Record ──> A-Record ──> D-Record       |
|                                                                     |
|  TrajectoryAnalyzer ──> I-Record ──> $metadata enrichment           |
+---------------------------------------------------------------------+
```

**Specification layer.**  Three declarative artifacts define an application:
a CSDL data model (entity types, relationships, actions), I/O Automaton specs
(states, transitions, guards, invariants), and Cedar policies (authorization
rules).  Nothing in this layer is imperative code.  A four-level verification
cascade (Section 5) validates these specifications before they reach the runtime.

**Actor runtime.**  Each entity instance is a lightweight actor that processes
messages sequentially -- one at a time, no concurrent state access.  This
sequential consistency guarantee simplifies reasoning about state machine
correctness: if the transition table is correct and the initial state is
valid, then every reachable state is valid.  Actor state is persisted through
event sourcing to PostgreSQL; periodic snapshots reduce replay time on restart.

**HTTP surface.**  The actor runtime is fronted by an HTTP API derived from the
CSDL data model.  Agents interact with it through standard HTTP verbs:
`GET` for queries, `POST` for actions, and `$metadata` for schema discovery.
The API is self-describing -- an agent can read the metadata endpoint and
discover the full surface without external documentation.

**Feedback loop.**  Production telemetry flows back into the system through
the Evolution Engine.  Unmet user intents become observation records, which
surface as structured proposals for specification changes.  Approved changes
run through the verification cascade and deploy via hot-swap, closing the
loop between production behavior and system evolution.

The framework is implemented in Rust (edition 2024) as 16 crates plus a
reference application, totaling 440+ tests.

---

## 3. Specification Layer

### 3.1 CSDL as the Data Model Contract

The Common Schema Definition Language (CSDL) is the XML schema format
standardized by OASIS for OData v4 <sup>[1]</sup>.  The choice of OData over alternatives
like GraphQL is deliberate: GraphQL optimizes for human developer flexibility
(query what you want, shape the response), but that flexibility becomes a
liability for agents, which must reason about query structure, cost, and
output validity on every call.  OData's rigid entity model -- fixed entity types,
declared actions, navigable relationships -- is more constrained, but constraints
are exactly what agents need.  The `$metadata` endpoint returns a complete,
machine-parseable XML contract; an agent can discover the full API surface,
available actions, and state machine annotations without documentation or
examples.  Temper uses CSDL as the single source
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
on the Lynch-Tuttle formalism <sup>[8]</sup>.  We chose I/O Automata over TLA+ or plain
finite state machines because the formalism's precondition/effect structure maps
directly to the runtime evaluation model (guard check, then apply effects),
and the input/output/internal action classification maps naturally to the actor
model's message taxonomy (environment messages, emitted events, private
transitions).  An I/O Automaton is a labeled state
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
- **Integration declarations:** `[[integration]]` sections declare external
  side effects (webhooks, notifications) as metadata, dispatched asynchronously
  after transitions (see Section 4.3.1).

The TOML serialization is parsed into an `Automaton` struct that feeds
both the verification cascade (directly to `TemperModel` for Stateright model
checking, simulation, and property tests) and the runtime `TransitionTable`.
The precondition/effect style maps directly to the `TransitionRule` guards and
effects used at runtime, and the input/output/internal classification maps to
the actor model's message taxonomy.

### 3.3 Cedar ABAC as the Security Specification

Authorization is modeled using Amazon's Cedar policy language <sup>[2]</sup>.  Cedar was
chosen over XACML (verbose XML, poor tooling) and Zanzibar-style relationship
models (designed for graph-shaped data, not entity state machines) because its
`(principal, action, resource, context)` tuple maps directly to the OData
request structure, and its policy language is amenable to formal analysis.  Cedar
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
Periodic snapshots reduce replay time on actor restart.  The `temper-store-redis`
crate defines traits for mailbox streams, shard placement, and TTL-based caching
with in-memory stub implementations.  Actor mailboxes currently use local
`tokio::sync::mpsc` channels; Redis-backed implementations are planned for
distributed deployment.

### 4.3.1 Integration Engine and Outbox Pattern

External integrations are declared as metadata in the IOA specification
(`[[integration]]` sections) and dispatched asynchronously after state transitions
via the `IntegrationEngine`.  The state machine itself remains pure and
deterministically verifiable: the verification cascade operates on transition
rules only and ignores integration metadata.  This separation is deliberate--
the outbox pattern ensures that side effects cannot violate state machine
invariants because they execute *after* the transition is persisted, not
during guard evaluation or effect application.

The dispatch flow is: `EntityActor` applies a transition, emits
`Effect::EmitEvent`, the event is persisted to the Postgres journal, and the
`IntegrationEngine` asynchronously dispatches to registered webhook endpoints.
Delivery follows at-least-once semantics with configurable exponential backoff
retry, matching the actor runtime's existing supervision backoff strategy.

### 4.4 Bounded Execution (TigerStyle)

Following TigerStyle's bounded execution principle <sup>[19]</sup>, the actor runtime
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

## 5. Four-Level Verification Cascade

TigerStyle <sup>[19]</sup> holds that "assertions are not just for testing--they run in
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

Correctness is established through a four-level cascade, orchestrated by the
`VerificationCascade` type in `temper-verify`.  All levels run independently;
all must pass before deployment.

### 5.1 From Specification to Verification Model

The I/O Automaton TOML specification is translated directly into a
`TemperModel` that implements the Stateright `Model` trait <sup>[3]</sup>.  The `Automaton` struct parsed
from TOML maps 1:1 to the model's structures.

The verification model tracks multi-variable state:

```rust
pub struct TemperModelState {
    pub status: String,                    // e.g. "Draft", "Submitted"
    pub counters: BTreeMap<String, usize>, // e.g. {"items": 2}
    pub booleans: BTreeMap<String, bool>,  // e.g. {"has_address": true}
}
```

Guards and effects from the IOA spec are translated directly:
- `Guard::MinCount { var, min }` -> `ModelGuard::CounterMin { var, min }`
- `Guard::IsTrue { var }` -> `ModelGuard::BoolTrue(var)`
- `Effect::Increment { var }` -> `ModelEffect::IncrementCounter(var)`
- `Effect::SetBool { var, value }` -> `ModelEffect::SetBool { var, value }`

Invariant assertions are classified automatically from their expression:
- `"items > 0"` -> `CounterPositive { var: "items" }`
- `"payment_captured"` (bare bool identifier) -> `BoolRequired { var }`
- `"no_further_transitions"` -> `NoFurtherTransitions`
- A `TypeInvariant` (status in valid set) is always auto-included.

Counter variables are bounded for finite exploration (default: 2).

### 5.2 From Specification to Runtime TransitionTable

The same IOA spec produces the runtime `TransitionTable` (in `temper-jit`),
which is a data structure--not compiled code:

```rust
pub struct TransitionTable {
    pub rules: Vec<TransitionRule>,  // Guard + Effect lists per action
}
```

The `evaluate_ctx()` method interprets these rules at runtime, checking guards
and applying effects.  The table is serializable and hot-swappable via
`Arc<TransitionTable>`.  Both the verification model and the runtime table
derive from the same `Automaton`, ensuring provable equivalence.

### 5.3 Level 0: Symbolic Verification

When state spaces are too large for BFS, Level 0 verifies properties
algebraically using the Z3 SMT solver.  Each guard and invariant is encoded
as a Z3 formula over bounded integer variables and booleans:

1. **Guard satisfiability:** For each transition, check whether its guard can
   ever be satisfied given the variable domains.  Unsatisfiable guards indicate
   dead code (an action that can never fire).

2. **Invariant induction:** For each (invariant, transition) pair: assuming the
   invariant holds before the transition and the guard is satisfied, does the
   invariant hold after applying the transition's effects?  If all pairs verify,
   the invariant holds inductively--no BFS needed.

3. **Unreachable state detection:** BFS from the initial state through
   transition targets to find states that cannot be reached.

```
L0 Symbolic PASSED: 10 guards satisfiable, 5 invariants inductive, 0 unreachable
```

Level 0 complements Level 1: it handles unbounded variable domains where BFS
would explode, while Level 1 provides exhaustive path coverage within bounds.

### 5.4 Level 1: Exhaustive Model Checking

Stateright performs breadth-first exhaustive exploration of the multi-variable
state space, checking safety and liveness properties at every reachable state.

**Safety properties** (`Property::always`):
- `TypeInvariant`: status in valid set
- `CounterPositiveInvariants`: when triggered, counter > 0
- `BoolRequiredInvariants`: when triggered, boolean is true
- `NoFurtherTransitions`: when triggered, no actions are enabled
- `ImplicationInvariants`: when triggered, status in required set

**Liveness properties:**
- `Property::eventually` for `ReachesState` (acyclic paths only)
- `Property::always` for `NoDeadlock` (always has enabled actions)

```
L1 Model Check PASSED: states explored, all properties hold
```

This level provides the strongest guarantee: no reachable state violates any
safety invariant.  The trade-off is that state-space explosion limits counter
bounds to small values.

### 5.5 Level 2: Deterministic Simulation

Inspired by FoundationDB's simulation testing <sup>[4]</sup> and TigerBeetle's VOPR <sup>[5]</sup>,
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
all safety properties.  Multi-seed runs sweep across seeds for broader coverage:

```
L2 Simulation PASSED: 10 seeds, 847 transitions, 23 dropped msgs
```

Crucially, any failure is reproducible: re-running with the same seed
produces the identical execution trace.

### 5.6 Level 3: Property-Based Testing

Level 3 uses proptest <sup>[6]</sup> to generate random action sequences and check
invariants after each step.  This complements Levels 0-2: Level 0 is algebraic
but per-step; Level 1 is exhaustive but bounded; Level 2 simulates realistic
multi-actor scenarios; Level 3 exercises long action sequences that may exceed
the model checker's state-space budget.  When a violation is found, proptest's
shrinking algorithm reduces the sequence to a minimal counterexample.

```
L3 Property Tests PASSED: 1000 cases, 30 max steps
```

### 5.7 What Each Level Catches

| Level | Method | Scope | Strengths |
|-------|--------|-------|-----------|
| L0 | Symbolic/algebraic | Unbounded variables | Dead guards, non-inductive invariants, unreachable states |
| L1 | Exhaustive BFS | Bounded, all variables | All reachable states, safety + liveness, counterexamples |
| L2 | Fault injection DST | Real code, multi-actor | Message delays/drops/crashes, concurrency bugs |
| L3 | Random walks + shrinking | Long sequences | Sequences beyond BFS budget, minimal counterexamples |

### 5.8 Faithfulness and Limitations

**IOA deviations from Lynch-Tuttle formalism:**
- *Input-enabledness* is violated: input actions have `from` guards.
- The guard/effect language is restricted to counters and booleans.
- No composition calculus or simulation relations.

**Stateright limitations:**
- `Property::eventually` only checks acyclic paths (no cycles).
- No fairness assumptions: liveness cannot express "if action A is enabled
  infinitely often, it eventually fires."

**SMT verification limitations:**
- Invariant induction proves per-step preservation, not reachability from
  initial state (BFS still needed for base case).
- The guard/effect language restricts what Z3 can reason about: only
  integer counters and booleans, no arithmetic expressions or set operations.

**Integration engine limitations:**
- Webhook is the only supported transport; gRPC and message queue transports
  are planned.
- Delivery is at-least-once; exactly-once semantics require idempotent receivers.

**Future work:** composition calculus for multi-entity verification,
fairness-aware liveness checker, richer guard language with arithmetic.

### 5.9 Cascade Orchestration

The `VerificationCascade` builder accepts an I/O Automaton specification and
configuration parameters, then runs all four levels and produces a
`CascadeResult`:

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

2. **P-Record (Problem):** A Lamport-style formal problem statement <sup>[7]</sup> derived
   from the observation.  It specifies the problem precisely--invariants that
   must continue to hold, constraints on the solution space, and an impact
   assessment (affected users, severity, trend).

3. **A-Record (Analysis):** Root cause analysis with one or more `SolutionOption`s.
   Each option specifies a specification diff (CSDL changes, automaton spec changes, Cedar
   policy changes), the impact on safety invariants, a risk level, and an
   estimated complexity.

4. **D-Record (Decision):** A human approval or rejection of a proposed change.
   If approved, the record includes verification cascade results (SMT
   invariant induction, Stateright states explored, simulation pass/fail,
   proptest cases) and an implementation plan (codegen command, migration
   required, deployment strategy).

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
   all transition rules for an entity type, built directly from the `Automaton`
   specification.  Each `TransitionRule` carries a `Guard` (evaluated at
   runtime against an `EvalContext` of counters and booleans) and a list of
   `Effect`s (`SetState`, `IncrementCounter`, `DecrementCounter`, `SetBool`,
   `EmitEvent`).  Transitions are evaluated by matching the action name,
   checking `from_states`, evaluating the guard, and applying effects.

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

In an agentic system and Temper specifically, this problem is worse: agents write I/O Automaton specs.  The platform must handle all observability without relying on the coding agent's subjective (non-determinstic) involvement in deciding metrics vs traces vs logs, or being locally reactive to a particular fault scenario.

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
different collector sends telemetry to Datadog, or any
OTLP-compatible backend without code changes.

### 9.4 Cost of cardinality Decoupling

The `entity_id` is high-cardinality (one per entity).  In
traditional telemetry, adding it as a metric tag causes cardinality explosion. With Telemetry as Views, `entity_id` is an Attribute, full detail in traces.  An Agentic operator can *promote* an Attribute to a Tag at runtime if they decide the cost is worth it for a specific investigation (using the O-P-A-D-I evolution), without any code change.

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

**Actor systems.**  Erlang/OTP <sup>[9]</sup> established the actor model for
fault-tolerant distributed systems with supervision trees and let-it-crash
semantics.  Akka <sup>[10]</sup> brought the model to the JVM with typed actors and
cluster sharding.  Microsoft Orleans <sup>[11]</sup> introduced virtual actors with
automatic activation and placement.  Temporal <sup>[12]</sup> provides durable execution
for long-running workflows.  Temper draws supervision and sequential message
processing from Erlang/Akka but adds specification-driven code generation and
a verification cascade absent from all of these systems.

**Formal verification.**  TLA+ <sup>[7]</sup> is the standard for specifying and
model-checking concurrent and distributed systems.  Alloy <sup>[13]</sup> provides
relational modeling with SAT-based analysis.  Stateright <sup>[3]</sup> brings
model checking to Rust with an API designed for testing distributed protocols.
FoundationDB's simulation testing framework <sup>[4]</sup> pioneered seed-based
deterministic simulation for database systems; TigerBeetle's VOPR <sup>[5]</sup> applies
the same idea to a financial transaction engine.  Temper combines Z3 SMT solving, Stateright
model checking, FoundationDB-style simulation, and proptest-based property
testing into a unified four-level cascade.

**TigerStyle.**  TigerBeetle's TigerStyle <sup>[19]</sup> is a development methodology that
codifies principles Temper adopts throughout its design: assertion density
(minimum two assertions per function), bounded execution (no unbounded loops,
no unbounded allocations), static resource budgets (all buffers sized at
initialization), and a "zero technical debt" philosophy where every shortcut is
treated as a bug.  TigerStyle elevates deterministic simulation testing from an
optional technique to the *primary* testing strategy, ahead of integration and
end-to-end tests.  Temper's four-level verification cascade, bounded actor
mailboxes, static transition tables, and DST-first development methodology are
direct applications of TigerStyle principles to the actor-framework domain.

**API frameworks.**  OData v4 <sup>[1]</sup> standardizes entity data models, query
conventions, and metadata.  GraphQL <sup>[14]</sup> provides a flexible query language
but lacks the formal entity model that Temper requires.  REST frameworks
(Rails, Django, Express) provide routing and ORM but no specification-level
verification.  Section 3.1 discusses why OData's rigidity is an advantage
for agent consumption.

**Attribute-based access control.**  XACML <sup>[15]</sup> defined the original ABAC
standard.  Google Zanzibar <sup>[16]</sup> provides relationship-based authorization
at scale.  Temper uses Amazon Cedar <sup>[2]</sup>; Section 3.3 discusses the rationale.

**Self-optimizing systems.**  CockroachDB <sup>[17]</sup> performs automatic range
splitting and rebalancing.  Neon <sup>[18]</sup> adjusts compute and storage resources
based on workload.  Temper's optimizer actors operate at the application layer,
adjusting query plans, cache policies, and actor placement based on
observability data, with a safety checker ensuring correctness.

---

## 11. Evaluation

### 11.1 Test Coverage

The Temper workspace contains 450 tests across 16 crates and one reference
application. Key categories:

| Category                       | Count | Crates                                     |
|--------------------------------|------:|--------------------------------------------|
| Actor runtime and scheduler    |    19 | temper-runtime                             |
| Specification parsing          |    20 | temper-spec                                |
| OData query/path parsing       |    35 | temper-odata                               |
| Code generation                |     6 | temper-codegen                             |
| Verification (SMT+model+sim+prop) |  38 | temper-verify                              |
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

**3 cascade tests.**  The `ecommerce_cascade.rs` test file runs the full four-level
`VerificationCascade` on each entity spec independently, confirming that SMT
verification, Stateright model checking, deterministic simulation, and
property-based testing all pass.

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

**Self-hosted .**  The coding agent produces a Cargo crate with
specs, full verification tests (cascade + DST), and infrastructure (Docker Compose).
The coding agent can also build and deploy the binary to the developer's preferred 
hosting provider (ex: railway, etc).  The reference e-commerce application is
an example.  This path gives the developer full control over the build,
deployment, and infrastructure.

**Platform-hosted .**  `temper serve --specs-dir ./specs --tenant
my-app` starts the server.  The serve command runs `VerificationCascade::from_ioa()`
on every IOA spec before loading it into the SpecRegistry; invalid specs are rejected
at startup, never loaded into the runtime.  Once loaded, the full OData API is live.
Multiple application tenants coexist on a single server, each with independently
verified specs dispatched through a shared `SpecRegistry`.

**Multi-tenancy.**  Both paths support multi-tenancy.  A system tenant provides
shared infrastructure.  All tenants dispatch through a shared `SpecRegistry` that
maps `(TenantId, EntityType)` to the verified `TransitionTable` and spec metadata.
Postgres events are tenant-scoped.  The Redis key naming convention
(`temper:{subsystem}:{tenant}:...`) is defined but not yet active.

**E2E proof.**  The `compile_first_e2e` tests prove the full HTTP lifecycle through
the platform-hosted path: entity creation, action dispatch, entity read, `$metadata`
retrieval, and service document discovery.  The `platform_e2e_dst` tests prove
multi-tenant isolation: two tenants with different entity types coexist on the same
server, each seeing only their own entities and actions.

**Conversational development (vision, partially implemented).**  The Developer
Chat pipeline--interview, spec generation, verification, hot-deploy--is partially
implemented in `temper-platform` and dependent on the coding agent of choice
(claude-code, cursor, etc) at the agent integration layer.  See Section 12.2 for
the target experience and two-context separation model.

### 11.4 Verification Results

Running the full verification cascade on each entity specification:

**Order** (10 states, 14 actions, 4 invariants):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L0    | Z3 SMT symbolic           | PASSED  | 10 guards satisfiable, 5 invariants inductive |
| L1    | Stateright model check    | PASSED  | All properties hold (multi-variable state)  |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, 847 transitions, 23 dropped msgs |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

**Payment** (6 states, 7 actions, 3 invariants):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L0    | Z3 SMT symbolic           | PASSED  | All guards satisfiable, invariants inductive |
| L1    | Stateright model check    | PASSED  | All properties hold (3 terminal-state invariants) |
| L2    | Deterministic simulation  | PASSED  | 10 seeds, all invariants held              |
| L3    | Property-based tests      | PASSED  | 1,000 cases, 30 max steps per case         |

**Shipment** (7 states, 7 actions, 1 invariant):

| Level | Method                    | Result  | Detail                                    |
|-------|---------------------------|---------|-------------------------------------------|
| L0    | Z3 SMT symbolic           | PASSED  | All guards satisfiable, invariants inductive |
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

TigerStyle <sup>[19]</sup> mandates that deterministic simulation testing is not an
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

Event replay uses the `TransitionTable` as the authoritative source of effects:
`replay_events()` re-evaluates each persisted event through `evaluate_ctx()`,
applying the same effect logic as the original `handle()` call.  This
reconstructs all state variables--status, named counters, and booleans--
without storing effects in the event payload.  The event journal stores
`(action, from_status, to_status, params)`; the `TransitionTable` provides
the effects deterministically.

The guard resolution fix introduced `TransitionTable::from_ioa_source()` which
builds the table directly from the parsed `Automaton` — the same data structure
used by the verification model.  This establishes a critical invariant:

> **The transition table in the HTTP-serving entity actor derives from the
> same `Automaton` verified by the four-level cascade.**

### 11.7 Performance Benchmarks

All benchmarks use Criterion 0.5 with 100 samples.  The e-commerce agent checkout
benchmark exercises the full stack an agent hits: HTTP request → axum routing →
OData path parsing → Cedar authorization → actor dispatch → TransitionTable
evaluation → Postgres event persistence → JSON response serialization.

**Transition table hot path.**  The `evaluate_ctx()` method uses a pre-built
`BTreeMap<String, Vec<usize>>` index for O(log K) action lookup by name,
eliminating the O(N) linear scan and `Vec` allocation on every call.

| Benchmark | Latency |
|-----------|---------|
| `evaluate_ctx` — successful transition | 28 ns |
| `evaluate_ctx` — guard failure | 30 ns |
| `evaluate_ctx` — wrong state (no match) | 25 ns |
| `evaluate_ctx` — unknown action | 14 ns |
| `TransitionTable::from_ioa_source()` (parse + compile + index) | 16 μs |
| `EvalContext` construction (2 counters, 1 boolean) | 149 ns |
| `rebuild_index()` | 669 ns |

**End-to-end agent checkout — in-memory (no persistence).**  Each checkout
executes 13 OData HTTP POST requests across 3 entity types (Order, Payment,
Shipment), driving the full order lifecycle from Draft through Delivered.

| Scenario | Latency |
|----------|---------|
| 1 agent checkout (13 actions, 3 entities) | 461 μs |
| 10 concurrent checkouts | 999 μs |
| 100 concurrent checkouts (1,300 requests, 300 actors) | 9.5 ms |

**End-to-end agent checkout — with PostgreSQL persistence.**  Same workload
with every event persisted to PostgreSQL 18 via `sqlx`.

| Scenario | Latency | Throughput |
|----------|---------|------------|
| 1 agent checkout (13 actions, 3 entities) | 17.7 ms | ~55 checkouts/sec |
| 10 concurrent checkouts | 62 ms | ~160 checkouts/sec |
| 100 concurrent checkouts (1,300 requests, 300 actors) | 591 ms | ~170 checkouts/sec |

The 100-concurrent result represents 1,300 persisted OData actions completing in
under 600 ms — approximately **2,200 persisted actions per second** through the
full HTTP stack on a single node.

**Bottleneck analysis.**  Postgres event append dominates at ~1.4 ms per action,
roughly 50× the in-memory actor dispatch path (~28 μs).  The `evaluate_ctx()` hot
path at 28 ns is effectively free relative to I/O.  OTEL trace export is
asynchronous (batch processor) and does not appear on the critical path.
Optimization priorities for higher throughput: write batching (multiple events per
round-trip), connection pool tuning, and snapshot-based recovery to reduce journal
replay on actor restart.

### 11.8 Live Infrastructure Validation

The system runs end-to-end against real infrastructure: PostgreSQL 18 for event
sourcing, an OpenTelemetry Collector for telemetry
ingestion, and ClickHouse as the observability backend.  The Docker Compose
environment provisions all four services.

The OTEL write path described in Section 9 is verified end-to-end: entity actor
transitions emit spans and metrics via the wide event projections, which flow
through the OTEL SDK's batch processor, out via OTLP/HTTP to the collector, and
into ClickHouse's `otel_traces` table.  Span durations in ClickHouse match
benchmark latencies (2--17ms per action with Postgres), confirming that the
telemetry pipeline introduces no measurement distortion.

As described in Section 9.3, the write path is backend-agnostic via
`OTLP_ENDPOINT` configuration.

---

## 12. Conclusion and Future Work

### 12.1 Summary of Contributions

Temper makes the following contributions:

1. A conversation-first architecture where specifications are generated from
   developer interviews, code is derived from specifications, and the system
   self-evolves from production trajectory intelligence.

2. A four-level verification cascade combining Z3 SMT symbolic verification,
   exhaustive model checking with multi-variable state and liveness properties,
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

8. Adoption of TigerStyle <sup>[19]</sup> as a cross-cutting engineering methodology:
   assertion density at the state machine level, bounded execution throughout
   the actor runtime, static resource budgets, and DST-first development
   where simulation testing is the primary--not supplementary--testing
   strategy.

9. A reference e-commerce application that demonstrates the full self-hosted
   development flow: three verified entity state machines (Order, Payment,
   Shipment), 22 DST tests with determinism proofs, three cascade tests,
   infrastructure as code, and a complete O-P-A-D-I evolution chain showing
   how production observations lead to spec improvements.

### 12.2 Conversational Development Vision

The development loop described in Section 11.3 (converse, generate, verify, deploy)
is designed around two separated contexts: a Developer Chat for design-time evolution
and a Production Chat for runtime operation.  The conversational pipeline is partially
implemented in `temper-platform` (interview agent, spec generators, verify-and-deploy
pipeline); the gap is the coding agent integration layer.

A representative session showing the target experience:

```
Developer: "I want to build something like Linear for my team"
System:    "What's the main thing people create and track?"
Developer: "Issues. They go through a workflow."
System:    "What stages does an issue go through?"
Developer: "Backlog, todo, in progress, in review, done.
            You can cancel from anywhere except done."
System:    "Got it. Here's what I'll set up:

            Issue lifecycle:
              Backlog → Todo → InProgress → InReview → Done
              Cancel from any active state → Cancelled (final)

            Actions: CreateIssue, Triage, StartWork, SubmitReview,
                     ApproveReview, CancelIssue

            I'll also enforce that cancelled issues can't be
            reopened. Verifying now..."

           ✓ Verified. Issue tracker is live.
           "Want to try it?"

Developer: "Create a bug for the login page"
System:    "Created ISS-1 'Login page bug' in Backlog."

Developer: "I need to assign issues to people"
System:    "Issues don't have assignments yet. Should I add that?
            Which stages can you assign from?"
Developer: "Any stage except Done and Cancelled"
System:    ✓ "Added assignments. Verified. Try assigning ISS-1."
```

The default experience hides all internals.  The system translates natural
descriptions into formal specifications, runs the four-level verification
cascade, and surfaces only the result: verified and live, or a plain-language
explanation of what went wrong.

When verification fails, the system explains the *domain* problem, not the
formal violation:

```
Developer: "Actually, let done issues be cancelled too"
System:    "I can't do that — you said cancelled is final, so
            allowing Cancel from Done would create a conflict.
            Should I remove the 'cancelled is final' rule, or
            keep it and only allow cancel from active stages?"
```

#### Progressive Disclosure of Internals

The analogy is `EXPLAIN ANALYZE` in PostgreSQL or `rustc --emit=mir`: the
default output is what matters to the user; the internals are one question
away.  A developer who wants to understand *why* something was verified, or
*how* the state machine is structured, can ask:

```
Developer: "Show me what you generated"
System:    "Issue entity (issue.ioa.toml):
              States: Backlog, Todo, InProgress, InReview, Done, Cancelled
              Actions:
                CreateIssue       → Backlog
                Triage            Backlog → Todo
                StartWork         Todo → InProgress
                SubmitReview      InProgress → InReview
                ApproveReview     InReview → Done
                CancelIssue       [Backlog,Todo,InProgress,InReview] → Cancelled
              Invariants:
                CancelledIsFinal  no transitions out of Cancelled"

Developer: "Show me the verification details"
System:    "Verification cascade for Issue:
              L0 SMT (Z3):      6 guards satisfiable, 2 invariants inductive
              L1 Model Check:   18 states explored, all properties hold
              L2 Simulation:    10 seeds, 312 transitions, 0 violations
              L3 Property Tests: 1000 cases, 30 max steps, all passed"

Developer: "Why did the cancel-from-done change fail?"
System:    "The NoFurtherTransitions invariant on Cancelled
            means no action can have Cancelled as a from-state.
            But CancelIssue targets Cancelled, and you want it
            to fire from Done — which means Done → Cancelled is
            a valid transition. That's fine. The problem is that
            if any future action were added from Cancelled, it
            would violate the invariant. The L1 model checker
            found this: in the state (Done, items=1), CancelIssue
            fires, reaching Cancelled — but the invariant requires
            no enabled actions from Cancelled, and currently none
            exist, so it actually passes. Let me re-check...

            Actually, the real conflict is: you said 'cancel from
            anywhere except Done' earlier, and 'cancelled is final'
            is already enforced. Adding Done to the cancel-from list
            works — I was wrong. Updated and verified. ✓"
```

The three levels of disclosure mirror compiler/database conventions:
1. **Result only** (default): "✓ Verified" or a domain-level explanation
2. **Spec summary** ("show me what you generated"): entity structure in
   plain language, no TOML syntax
3. **Cascade details** ("show me verification details"): L0-L3 results,
   state counts, counterexample traces — the `EXPLAIN ANALYZE` view

#### Two-Context Separation

The system maintains two separated contexts:

1. **Developer Chat** (design-time).  The developer builds and evolves the
   application through conversation.  The system interviews, generates specs,
   verifies, and deploys — the developer controls *what* the application
   does without needing to understand *how* verification works.

2. **Production Chat** (runtime).  End users interact with the deployed
   application.  The agent operates within the current specifications — it
   cannot modify the entity model.  When users attempt something the system
   can't do, the intent is captured and surfaced to the developer:

   ```
   User: "Split my order into two shipments"
   Agent: (no matching action) → "I can't do that yet"
   → Intent captured → Developer notified:
     "Users are asking to split orders (47 attempts this week).
      Should I add a SplitOrder action?"
   → Developer approves/rejects → If approved: verify → deploy
   ```

   The developer retains the approval gate for all behavioral changes.

#### Conversational Development Pipeline

The Developer Chat supports two interaction patterns:

**Interactive mode.**  The system and developer go back and forth, refining
one entity at a time.  Each exchange that implies a change triggers
generation, verification, and deployment immediately.

**Plan mode.**  The developer describes everything upfront: "I want a project
management tool with issues, projects, and sprints.  Issues go through
backlog, todo, in progress, review, done."  The system generates all entities
at once, verifies, and deploys.  Follow-up conversation refines incrementally.

Under the hood, both modes follow the same pipeline: the system translates
the conversation into IOA specifications, generates CSDL and Cedar policies,
runs the four-level verification cascade, and hot-deploys.  The developer
experiences this as "describe → try it" with verification as an invisible
gate that only surfaces when something is wrong.

### 12.3 Remaining Future Work

Several additional directions remain open:

- **Runtime deploy endpoint.**  An HTTP API for submitting specs at runtime
  would enable hot-reload without server restart.  The verification cascade
  would run server-side before registration, bridging the gap between the
  self-hosted path and the full conversational pipeline.

- **Distributed clustering.**  The current actor system is single-node;
  actor mailboxes are local `mpsc` channels.  The `temper-store-redis` crate
  defines `MailboxStore`, `PlacementStore`, and `CacheStore` traits with
  in-memory stubs, but Redis-backed implementations are not yet wired in.
  Extending `temper-runtime` with Redis-backed mailboxes, a cluster membership
  protocol, and remote actor references would enable horizontal scaling.

- **WASM actor bodies.**  Compiling actor message handlers to WebAssembly
  would allow hot-loading new actor logic without restarting the host process,
  complementing the JIT transition table layer.

- **Cloud deployment.**  Packaging the platform as a managed service with
  popular hosting platforms (vercel, railway, fly.io, cloudflare, cloud providers, etc).

- **Fairness-aware liveness.**  The current liveness checking uses
  Stateright's `Property::eventually` which only covers acyclic paths.
  Full fairness assumptions ("if enabled infinitely often, eventually fires")
  would require a custom checker or integration with a temporal logic tool.

- **Multi-entity composition.**  Verifying properties that span multiple
  entity types (e.g., "an order cannot be shipped unless its payment is
  captured") requires a composition calculus not yet implemented.

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

[8] N. Lynch and M. Tuttle. *An Introduction to Input/Output Automata.*
CWI Quarterly, 2(3):219-246, 1989.

[9] J. Armstrong. *Making Reliable Distributed Systems in the Presence of
Software Errors.* PhD Thesis, Royal Institute of Technology, Stockholm, 2003.

[10] Lightbend. *Akka: Build Concurrent, Distributed, and Resilient
Message-Driven Applications.* https://akka.io

[11] S. Bykov et al. *Orleans: Distributed Virtual Actors for
Programmability and Scalability.* Microsoft Research Technical Report
MSR-TR-2014-41, 2014.

[12] Temporal Technologies. *Temporal: Durable Execution Platform.*
https://temporal.io

[13] D. Jackson. *Software Abstractions: Logic, Language, and Analysis.*
MIT Press, 2012.

[14] Facebook. *GraphQL Specification.* 2015. https://spec.graphql.org

[15] OASIS. *eXtensible Access Control Markup Language (XACML) Version 3.0.*
OASIS Standard, 2013.

[16] R. Pang et al. *Zanzibar: Google's Consistent, Global Authorization
System.* USENIX ATC, 2019.

[17] CockroachDB. *CockroachDB: The Resilient Geo-Distributed SQL Database.*
https://www.cockroachlabs.com

[18] Neon. *Neon: Serverless Postgres.* https://neon.tech

[19] TigerBeetle. *TigerStyle: Engineering Design Philosophy.*
https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md

[20] OpenTelemetry. *OpenTelemetry Specification.*
https://opentelemetry.io/docs/specs/otel/
