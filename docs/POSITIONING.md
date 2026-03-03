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

## 6. The Agent Operating Layer

There's a stronger claim than "agents can generate specs" or "agents can consume the API." It's this: **Temper is the operating layer for autonomous agents.**

Agents today run with whatever tools they're given. They call APIs directly, write to databases, execute code in sandboxes. There is no shared governance model. There is no formal verification of what an agent is about to do. There is no audit trail that connects an agent's intent to its effects. When something goes wrong, you grep through logs.

The thesis: every state-changing action an agent takes should flow through a governed, verified, auditable layer. Not optionally. By design.

### The agent is both developer and operator

In the personal assistant and enterprise employee use cases, the agent builds its own specifications. When an agent needs to execute a multi-step plan -- process an expense report, coordinate a deployment, manage a customer interaction -- it generates an IOA specification describing the states, transitions, guards, and integrations of that plan. Temper verifies the spec through the four-level cascade before the agent can execute through it. The agent's plan itself is a verified state machine.

The agent then operates through the verified spec: calling actions, transitioning state, triggering integrations. The spec is the contract. The verification cascade is the proof. The runtime enforces the contract on every action.

### The human is the policy setter

Cedar policies define what agents can and cannot do. The default posture is deny-all. When an agent attempts something not yet permitted, the denial surfaces to the human: "Your agent tried to call the Stripe API and was blocked. Allow?" The human approves with a scope -- narrow, medium, or broad -- and Temper generates the Cedar policy and hot-loads it. Over time, the policy set converges on what the agent actually needs. The human doesn't anticipate permissions upfront; they respond as needs arise.

### Everything is recorded

Every action an agent takes through Temper is a state transition. Every transition is persisted with the agent's identity, the before/after state, whether authorization succeeded or was denied, and the Cedar policy that governed the decision. This gives you an audit trail, agent self-awareness (the agent can query its own state), and cross-agent visibility (multiple agents sharing a Temper instance see each other's state changes).

### External access is governed

When agents need to call outside systems, they do so through integrations declared in the IOA spec. Cedar policies govern which external calls are permitted. WASM modules for integrations run in a sandbox. In the vision, these modules can be reviewed by a security agent or formally verified -- the same way state machine specs are verified today.

### The interface is a REPL

The vision for how agents interact with Temper is a sandboxed code execution environment -- in the style of Symbolica's Agentica or Cloudflare's Code Mode. Agents write code against a typed API surface; the sandbox mediates all external access through Temper. The REPL is the only tool the agent is given for state-changing operations.

### What this means

Agents generating specifications is already possible -- the spec submission API and verification cascade exist today. Cedar default-deny governance, pending decision approval flows, per-agent audit trails, and the observe dashboard for agent activity -- these are built and working. The REPL interface and security review agents are the vision, not yet implemented.

The question is not whether agents need governance. The question is whether governance can be formal, verified, and transparent rather than ad hoc. That is what Temper is for.

## 7. The Evolution Loop

The most interesting part might be what happens after deployment.

Production usage generates trajectory data. When a user tries an action the current spec doesn't support, the system captures it as an observation record. These observations surface to the developer as structured proposals: "users are trying to split orders into multiple shipments; here's a spec diff that would enable it." The developer approves or rejects. Approved changes run through the verification cascade and deploy via hot-swap.

This creates a feedback loop between production behavior and system evolution. The developer stays in the approval seat, but the system does the discovery work. Over time, the specs converge toward what users actually need rather than what someone imagined at design time.

The O-P-A-D-I record chain (Observation, Proposal, Approval, Deployment, Impact) provides a complete audit trail for every behavioral change. It's early, but the loop is operational in the current implementation.

---

*This document describes the current state of the project. The five verified specs, the benchmark numbers, and the test counts reflect what exists today. The agent operating layer -- Cedar governance, pending decisions, audit trails -- is built and working. The REPL interface and security review agents are the next things to build. Whether this pattern holds across a broader set of real-world agent deployments is the next thing to find out.*
