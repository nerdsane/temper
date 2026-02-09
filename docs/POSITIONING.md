# Temper: Positioning

## 1. Thesis

Every enterprise SaaS is a state machine + integrations + authorization. If you accept that premise, the entire backend is derivable from a conversation.

An Order has states (Draft, Submitted, Shipped). A Subscription has states (Active, PastDue, Cancelled). An Approval has states (Drafted, PendingApproval, Approved). The business logic is the transitions between them, the guards that prevent invalid transitions, and the invariants that must always hold.

The rest -- persistence, API endpoints, authorization, observability, webhooks -- is infrastructure that follows mechanically from the state machine definition. Temper derives all of it from a single IOA TOML specification, verified before deployment.

## 2. The Decomposition

Every SaaS backend decomposes into six layers. Temper replaces each with a declarative primitive:

| Traditional Layer | Temper Primitive | Code Path |
|---|---|---|
| ORM models, migrations | CSDL XML | `temper-spec/src/csdl/parser.rs` auto-generates OData entity types |
| Controller code, service layers | IOA TOML specs | `TransitionTable::from_ioa_source()` in `temper-jit/src/table/builder.rs` |
| if/else workflow chains | TransitionTable rules | Guards, effects, from/to states evaluated by `TransitionTable::evaluate_ctx()` |
| Auth middleware | Cedar ABAC policies | Cedar policy engine evaluates `Action in [AllowedActions]` per principal/resource |
| Webhook/API integrations | Integration Engine | `[[integration]]` declarations in IOA TOML, outbox pattern via event journal |
| Manual instrumentation | Automatic WideEvent telemetry | `temper-observe/src/wide_event.rs` emits spans + metrics for every action |

The developer writes none of these layers. The conversational platform (`temper-platform`) interviews the developer, generates all specs, and the verification cascade validates them before deployment.

## 3. Pattern Mapping

Five IOA specifications demonstrate the pattern across different business domains. Each follows the same structure: states, guards, effects, invariants, integrations.

### 3.1 E-Commerce Order (`reference-apps/ecommerce/specs/order.ioa.toml`)

**Context**: A 10-state order lifecycle handling the full flow from draft through delivery, cancellation, and returns.

```
Draft --> Submitted --> Confirmed --> Processing --> Shipped --> Delivered
  |          |             |                                       |
  +----+-----+             |                              ReturnRequested
       |                   |                                       |
   Cancelled          Cancelled                               Returned
                                                                   |
                                                               Refunded
```

**Key invariants**:
- `SubmitRequiresItems`: No empty orders reach Submitted (guard: `items > 0`)
- `CancelledIsFinal` / `RefundedIsFinal`: Terminal states have no outbound transitions

**Integrations**: `notify_fulfillment` on SubmitOrder, `charge_payment` on ConfirmOrder, `notify_shipping` on ShipOrder.

This is the most complex spec in the reference app and exercises multi-state cancellation (Draft, Submitted, or Confirmed), counter guards, and the full integration webhook pattern.

### 3.2 Support Ticket (`test-fixtures/specs/ticket.ioa.toml`)

**Context**: Customer support workflow with agent assignment, back-and-forth replies, and resolution tracking.

```
Open --> InProgress <--> WaitingOnCustomer
              |
          Resolved --> Closed
              |
             Open (reopen)
```

**Key invariants**:
- `ClosedIsFinal`: Closed tickets cannot be reopened
- `ResolvedNeedsReply`: Cannot resolve without at least one agent reply (`replies > 0`)

**Liveness**: From Open, the ticket eventually reaches Resolved or Closed. The `replies` counter and `customer_responded` boolean track engagement state independently of the status lifecycle.

### 3.3 Approval Workflow (`test-fixtures/specs/approval.ioa.toml`)

**Context**: Document or change request approval with reviewer assignment, rejection/revision cycles, and withdrawal.

```
Drafted --> PendingApproval --> Approved
   ^              |
   |           Rejected
   |              |
   +--(Revise)----+
   |
Withdrawn <-- Drafted/PendingApproval
```

**Key invariants**:
- `ApprovedHasApproval`: Approved state requires `approvals > 0`
- `WithdrawnIsFinal`: Withdrawal is permanent

The boolean guard pattern (`is_true has_reviewer`) prevents submission without a reviewer -- a common business rule. Revise resets `has_reviewer` to false, forcing reassignment before re-submission.

### 3.4 Subscription Management (`test-fixtures/specs/subscription.ioa.toml`)

**Context**: SaaS subscription with payment failure escalation, suspension, and cancellation.

```
Active <-- RetryPayment -- PastDue --> Suspended --> Expired
  |                           |            |
  +------+--------------------+------------+
         |
     Cancelled
```

**Key invariants**:
- `CancelledIsFinal` / `ExpiredIsFinal`: Terminal states
- `PastDueHasFailure`: The `payment_failures` counter must be > 0 whenever in PastDue, Suspended, or Expired

**Integrations**: `billing_webhook` fires on PaymentFailed, `dunning_notice` fires on SuspendSubscription. Self-transitions (`EnableAutoRenew`, `DisableAutoRenew`) modify the `auto_renew` boolean without changing status -- demonstrating that state variables and status are orthogonal.

### 3.5 Issue Tracker (`test-fixtures/specs/issue.ioa.toml`)

**Context**: Project management issue lifecycle with assignee tracking and review cycles.

```
Backlog --> InProgress --> Review --> Done --> Archived
   ^           ^             |         |
   |           +---(changes)-+         |
   |           +-------(reopen)--------+
```

**Key invariants**:
- `ArchivedIsFinal`: Archived issues cannot be reopened
- `StartRequiresAssignee`: InProgress/Review/Done states assert `assignee_set = true`

**Liveness**: From Backlog, issues eventually reach Done or Archived. The `review_cycles` counter tracks how many times code review occurs (both RequestChanges and ApproveReview increment it), providing a built-in velocity metric.

## 4. What Works Today

Every claim below maps to working code. Test counts from `cargo test --workspace`.

**Specification and parsing** (temper-spec, 16+ tests):
- IOA TOML parser: `parse_automaton()` in `crates/temper-spec/src/automaton/parser.rs`
- CSDL XML parser: `parse_csdl()` in `crates/temper-spec/src/csdl/parser.rs`
- Validation rejects invalid from/to states, missing initial states

**4-level verification cascade** (temper-verify, `crates/temper-verify/src/cascade.rs`):
- Level 0: SMT symbolic verification -- guard satisfiability, invariant induction, unreachable state detection
- Level 1: Stateright BFS exhaustive model checking -- explores every reachable state
- Level 2: Deterministic simulation with fault injection -- SimActorSystem with crash/restart
- Level 3: Property-based testing via proptest -- random action sequences

**Runtime** (temper-jit + temper-runtime + temper-server):
- `TransitionTable::from_ioa_source()` builds the runtime rule engine
- `SwapController` in `temper-jit/src/swap.rs` enables hot-swap of transition tables
- Entity actors with Postgres event sourcing (`temper-store-postgres`)
- Multi-tenant `SpecRegistry` in `temper-server/src/registry.rs`: `register_tenant()` maps (TenantId, EntityType) to specs
- OData API auto-generated from CSDL entity types

**Conversational platform** (temper-platform, 53+ unit tests, 9 E2E tests):
- Developer interview agent: Welcome through EntityDiscovery through SpecReview through Deployed
- Spec generators: `generate_ioa_toml()`, `generate_csdl_xml()`, `generate_cedar_policies()`
- Verify-and-deploy pipeline: parse, VerificationCascade, `register_tenant()`
- Production chat agent with dynamic system prompt from OData `$metadata`
- Evolution pipeline: `UnmetIntentCollector` captures production gaps

**Observability** (temper-observe):
- `WideEvent` automatic telemetry for every action execution
- OTEL SDK integration: spans + metrics via OTLP/HTTP
- ClickHouse read path for analysis and sentinel queries

**Authorization**:
- Cedar ABAC policy evaluation per action
- Policies generated alongside IOA and CSDL specs

**Deterministic simulation testing** (temper-verify):
- 16 E2E business simulation tests (9 scripted, 4 random, 3 determinism proofs)
- 6 rigorous determinism proofs: bit-exact replay across 10 runs with heavy fault injection
- `sim_now()` / `sim_uuid()` for fully deterministic time and IDs

**Workspace**: 16 crates, 446+ tests passing, zero failures.

## 5. Current Limitations

| Limitation | Impact | Workaround |
|---|---|---|
| No floating-point in state model | Cannot track prices/amounts as state variables | Use Postgres event payload fields for decimals; state model tracks status + counts |
| No cross-entity invariants | Cannot express "Order.status == Shipped implies Shipment.status != Created" | Integration engine orchestrates cross-entity coordination via action triggers |
| No conditional effects | Cannot express "if items > 5 then set bulk_discount true" | Decompose into multiple actions with different guards |
| Single-node only | Vertical scaling; no horizontal distribution | Redis store traits designed (`MailboxStore`, `PlacementStore`) but not wired; in-memory actors suffice for moderate load |
| No temporal guards | Cannot express "if last_payment > 30 days ago" | Integration engine schedules time-based actions via cron triggers |
| No UI layer | OData API only; no built-in frontend | OData is a standard -- any frontend framework (React, Vue, etc.) or low-code tool can consume it |
| Agent dependency for spec generation | Conversational platform requires an LLM (Claude, GPT, etc.) for interview and spec generation | Specs can be hand-authored in IOA TOML; the parser does not require an agent |
| No string-valued state variables | State model has status, counters, and booleans only | Use entity payload fields for strings; state model captures the finite automaton |

## 6. The Agent-Native Thesis

Temper is designed for a world where agents build and operate software.

**Agents generate specs.** The conversational platform (`temper-platform/src/deploy/pipeline.rs`) demonstrates the full loop: a developer describes what they want, the interview agent discovers entities and constraints, spec generators produce IOA TOML + CSDL + Cedar, and the verification cascade validates everything before deployment. The developer never writes code.

**Verification replaces code review.** The 4-level cascade (`temper-verify/src/cascade.rs`) is automated and exhaustive. SMT proves guard satisfiability symbolically. Stateright explores every reachable state. DST injects faults and verifies recovery. Proptest throws random sequences. No human reviewer can match this coverage. When verification fails, the system reports domain-level errors ("cancelled is final conflicts with cancel-from-done"), not internal implementation details.

**OData IS the agent-facing interface.** The OData API generated from CSDL is not just for human frontends -- it is a structured, self-describing API that agents can navigate via `$metadata`. Production chat agents in `temper-platform` already use it: they read entity state, discover available actions, and execute them through the same API.

**No code means no technical debt.** There are no controllers to refactor, no service layers to maintain, no ORM mappings to update. A spec change produces a new `TransitionTable`, verified and hot-swapped via `SwapController`. The old code simply ceases to exist.

**The system improves from production usage.** The evolution engine (`temper-platform/src/evolution/`) captures unmet user intents -- actions users try to perform that the current spec does not support. These become O-Records (observation records) that feed back to the developer for approval, closing the loop between production behavior and spec evolution.

## 7. The Evolution Loop

Production usage generates trajectory data:

```
User Action (attempted)
    |
    v
TransitionTable::evaluate_ctx()
    |
    +--> Success: WideEvent emitted, state updated
    |
    +--> Failure (action not available in current state):
            |
            v
        UnmetIntentCollector
            |
            v
        O-Record (observation: "user tried X in state Y")
            |
            v
        Developer Review (approval gate)
            |
            +--> Approve: I-Record generated, new spec version
            |         |
            |         v
            |     VerificationCascade
            |         |
            |         v
            |     D-Record (deployment: hot-swap via SwapController)
            |
            +--> Reject: intent logged, no spec change
```

The O-P-A-D-I record chain (Observation, Proposal, Approval, Deployment, Impact) creates a complete audit trail from user behavior through spec evolution to production deployment. Every change is verified before it reaches production. Every rejection is recorded for future analysis.

This loop means the system does not just run -- it learns what users need and surfaces those needs to developers in a structured, verifiable way. The developer remains the approval gate, but the system does the discovery work.
