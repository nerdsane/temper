# ADR-0178: Recorded System One judgments as IOA guards

- Status: Accepted
- Date: 2026-09-17
- Deciders: Temper maintainers, through the authorized implementation task
- Related: ADR-0149 (external guards), ADR-0046 (optimistic concurrency), ADR-0174 (action contracts)

## Context

IOA guards currently compare deterministic local state or externally resolved
cross-entity facts. Applications also need typed judgments over text and structured
context. TypeSafe's Jev API evaluates one `state` against named Choice, Score, and
Noul questions. A model call is external input, not a deterministic predicate.

## Decision

Add `type = "system_one"` inside the existing typed `guard = [...]` list. The
guard contains the API's `model`, `state`, and `questions`, plus a Temper `assert`
expression over `answers`. No new top-level specification sections or lifecycle
states are required. Existing guard entries remain conjunctive.

State bindings use `{ ref = "entity.Field" }` or `{ ref = "params.Parameter" }`.
Objects, arrays, and literals compose bindings recursively. V1 does not read related
entities or evaluate code. Missing inputs and oversized context are explicit errors.
Assertions support typed scalar comparisons and conjunction, not arbitrary code.
Decimal values use explicit fixed-point normalization for deterministic comparisons.

The shared spec translation carries the full guard to runtime and verification.
Runtime guard checks remain pure and consume trusted, pre-resolved evidence.
Only the kernel can construct that evidence; it cannot be deserialized from
application input or assembled through the public Rust API.
An injected provider owns HTTP; the simulation implements that same boundary.
Evaluate after inexpensive checks and outside the entity actor critical section.
Validate every answer against the question schema before testing assertions.

Evaluation receipts are event-store records separate from domain transitions. An
attempt is bound to tenant, principal, entity, action, parameters, entity precondition,
and specification identity. Persist positive and negative outcomes before using them.
Reserve the attempt independently of its guard declarations before inference, so
adding, removing, or reordering guards cannot resample a previously used attempt.
Completed retries validate caller, action, parameters, and spec while allowing the
entity to have advanced after its committed transition.
Retries reuse a durable receipt. Concurrent results use first-writer-wins append
semantics; a crash before recording can repeat inference, but cannot commit a domain
transition. Receipt references accompany committed transition events. Replay never
calls the provider. A deployment without durable storage cannot execute these guards.

Inside the actor, check the evaluated precondition and current specification before
applying effects, and again after any optimistic-concurrency catch-up. Missing or stale
evidence never enables an action. Unsupported runtime/composite execution paths must
explicitly refuse System One actions. Provider errors do not change domain state.

Credentials are tenant secrets named `TYPESAFE_API_KEY`. Local verification imports
the user's ignored `.env.typesafe.local` key into an isolated test tenant. Secrets
must not appear in logs, specification artifacts, or evaluation receipts.

## Verification and readiness gates

Treat model answers as external nondeterministic inputs. Explore possible successful
and unsuccessful guard outcomes; safety results do not establish model accuracy.
Report external-answer assumptions on liveness. Validate assertion consistency within
one response and statically reject malformed/unknown answer references.

DST harnesses must fail before implementation for absent, stale, duplicated, failed,
and hot-reloaded evaluations. Execute production guard/dispatch logic under simulated
provider and storage faults across many seeds. Follow with the L0-L3 cascade, affected
crate suites, live OData tests for all three primitives, and restart/replay proof.

## Rollout plan

1. Add failing harness scenarios, shared types, parsing, and pure assertion semantics.
2. Add the provider, durable receipts, dispatch resolution, and actor freshness checks.
3. Extend verification and explicitly gate unsupported execution surfaces.
4. Run simulation, suites, and real TypeSafe-backed local server verification.

## Consequences

The IOA can express semantic preconditions without moving application logic into the
kernel. External evaluation adds latency, cost, durable evidence, and potential denial
when the service is unavailable. State/spec changes invalidate in-flight judgments.
Explicit reevaluation uses a new logical attempt; negative retries do not resample.

### DST compliance

All I/O is behind provider/event-store interfaces. Ordered collections, `sim_now`,
and `sim_uuid` are used in simulation-visible execution. Seeded provider responses
are environmental inputs; guard and assertion evaluation are production code.

## Non-goals

Related-entity context assembly, arbitrary expression execution, probabilistic
transition scheduling, and guarantees that model judgments are factually correct.
