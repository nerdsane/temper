# Temper: An Observation About Enterprise SaaS

## 1. The Pattern

Spend enough time looking at enterprise SaaS backends and a pattern starts to emerge. An e-commerce order moves through Draft, Submitted, Confirmed, Shipped, Delivered. A support ticket goes from Open to InProgress to Resolved to Closed. A subscription cycles between Active, PastDue, Suspended, Cancelled.

The business logic in each case is a state machine: states, transitions between them, guards that prevent invalid transitions ("you can't submit an empty order"), and invariants that must always hold ("cancelled is final"). The entities are different, but the shape of the problem is the same.

What surrounds this core? Persistence, API endpoints, authorization, webhooks, observability. These layers are important -- critical, even -- but they follow mechanically from the state machine definition. If I know the states, transitions, and invariants, I can derive the rest.

This is not a new observation. State machines are a well-studied formalism. What's interesting is asking: *how far can you push this?* If the state machine is the essential artifact, what becomes possible?

## 2. What Falls Out

If you accept the premise that the state machine is the core, each layer of a traditional SaaS backend maps to a declarative primitive:

| What you'd normally write | What it maps to |
|---|---|
| ORM models, migrations | A CSDL data model |
| Controllers, service layers | IOA TOML specifications |
| if/else workflow logic | TransitionTable guards and effects |
| Auth middleware | Cedar ABAC policies |
| Webhook integrations | Integration declarations (outbox pattern) |
| Manual instrumentation | Automatic telemetry from transitions |

The question is whether this mapping is a useful simplification or an over-reduction that loses expressiveness. Temper is an attempt to find out.

## 3. Five Patterns

To test how far the IOA approach stretches, we wrote specifications for five different SaaS patterns. All five parse, verify through a four-level cascade (SMT symbolic checking, exhaustive model checking, deterministic simulation, and property-based testing), and run in the same actor runtime.

### E-Commerce Order (`reference-apps/ecommerce/specs/order.ioa.toml`)

The most complex spec: 10 states, 12 transition actions. Multi-state cancellation (from Draft, Submitted, or Confirmed). A counter guard (`items > 0`) prevents empty orders from reaching Submitted. Terminal states (Cancelled, Refunded) have no outbound transitions. Integration hooks fire on SubmitOrder, ConfirmOrder, and ShipOrder.

```
Draft --> Submitted --> Confirmed --> Processing --> Shipped --> Delivered
  |          |             |                                       |
  +----+-----+             |                              ReturnRequested
       |                   |                                       |
   Cancelled          Cancelled                               Returned
                                                                   |
                                                               Refunded
```

### Support Ticket (`test-fixtures/specs/ticket.ioa.toml`)

A back-and-forth workflow: agents reply, customers respond, the ticket bounces between InProgress and WaitingOnCustomer. The `replies` counter prevents resolution without engagement. Closed is terminal; Resolved can be reopened.

### Approval Workflow (`test-fixtures/specs/approval.ioa.toml`)

Boolean guards (`is_true has_reviewer`) prevent submission without a reviewer. Revise resets the boolean, forcing reassignment. The `approvals` counter proves approval happened.

### Subscription Management (`test-fixtures/specs/subscription.ioa.toml`)

Payment failure escalation: Active → PastDue → Suspended → Expired. Self-transitions (EnableAutoRenew, DisableAutoRenew) modify booleans without changing status, demonstrating that state variables and status are orthogonal. Integration hooks fire on PaymentFailed and SuspendSubscription.

### Issue Tracker (`test-fixtures/specs/issue.ioa.toml`)

Assignee tracking via boolean, review cycle counting. StartWork requires an assignee. Both RequestChanges and ApproveReview increment the review counter, giving a built-in velocity metric.

Each of these took minutes to write and passed the full verification cascade on the first or second attempt. The harder question -- whether this pattern library covers enough of the real-world design space to be useful -- remains open.

## 4. What Works

Everything below maps to working code. 441 tests pass across 16 crates.

- **IOA TOML parser** with six section types: automaton, state, action, invariant, liveness, integration
- **4-level verification cascade**: L0 SMT symbolic, L1 Stateright exhaustive, L2 deterministic simulation with fault injection, L3 proptest
- **Actor runtime** with Postgres event sourcing, hot-swap via SwapController, multi-tenant SpecRegistry
- **OData API** auto-generated from CSDL entity types
- **Conversational platform** that interviews developers, generates specs, and deploys through the cascade
- **Integration engine** (outbox pattern): webhooks dispatched asynchronously from the event journal
- **Automatic telemetry**: two-layer OTEL spans (HTTP + actor) with real durations verified in ClickHouse
- **Cedar ABAC** authorization evaluated per action
- **Evolution engine** that captures unmet user intents from production

Performance through the full OData HTTP stack with Postgres: ~28ns for rule evaluation, ~18ms per persisted action end-to-end, ~591ms for 100 concurrent checkouts (2,200 actions/sec).

## 5. What Doesn't Work (Yet)

| Gap | Why it matters | Current workaround |
|---|---|---|
| No floating-point state variables | Can't track prices as state | Use Postgres event payload fields |
| No cross-entity invariants | Can't express "Shipped implies Payment captured" | Integration engine orchestrates |
| No conditional effects | Can't do "if items > 5 then bulk discount" | Decompose into actions with guards |
| Single-node only | No horizontal scaling | Redis traits designed but not wired |
| No temporal guards | Can't do "if idle > 30 days" | Integration engine cron triggers |
| No UI layer | API only | OData is a standard; any frontend works |
| Spec gen needs an LLM | Interview agent requires Claude/GPT | Specs are hand-writable IOA TOML |
| No string state variables | Status + counters + booleans only | Finite automaton by design; strings in payload |

Some of these are fundamental to the approach (finite automaton = no strings in state). Others are engineering work (Redis wiring, temporal guards). Being clear about which is which matters.

## 6. The Agent Angle

There's a trendline worth paying attention to: agents are getting better at generating structured artifacts. Code, schemas, configurations. If you accept that trajectory, a few things follow.

Agents could generate the specifications. The conversational platform already demonstrates this path -- a developer describes their domain, and the system produces IOA TOML, CSDL, and Cedar policies. The verification cascade catches errors that neither the agent nor the developer would notice through inspection alone.

Verification could replace code review for this class of problems. A four-level cascade that explores every reachable state, injects faults, and throws random sequences is more thorough than human review of imperative code. When it fails, it reports domain-level explanations ("cancelled is final conflicts with cancel-from-done"), not stack traces.

The OData API is already agent-friendly. Self-describing via `$metadata`, structured, standard. Production agents in temper-platform use it today.

No generated code means no technical debt to accumulate. A spec change produces a new TransitionTable, verified and hot-swapped. The old logic simply ceases to exist.

Whether this adds up to something meaningful depends on how much of the enterprise SaaS design space actually fits the state machine pattern. The five specs above suggest the coverage is broader than you might expect. But five is not a proof.

## 7. The Evolution Loop

The most interesting part might be what happens after deployment.

Production usage generates trajectory data. When a user tries an action the current spec doesn't support, the system captures it as an observation record. These observations surface to the developer as structured proposals: "users are trying to split orders into multiple shipments; here's a spec diff that would enable it." The developer approves or rejects. Approved changes run through the verification cascade and deploy via hot-swap.

This creates a feedback loop between production behavior and system evolution. The developer stays in the approval seat, but the system does the discovery work. Over time, the specs converge toward what users actually need rather than what someone imagined at design time.

The O-P-A-D-I record chain (Observation, Proposal, Approval, Deployment, Impact) provides a complete audit trail for every behavioral change. It's early, but the loop is operational in the current implementation.

---

*Temper is a working system, not a vision document. The claims above are grounded in code paths and test counts. The open questions are genuine -- we don't know how far this approach extends, and we're curious to find out.*
