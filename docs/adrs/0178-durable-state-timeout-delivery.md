# ADR-0178: Durable state-timeout delivery

- Status: Accepted
- Date: 2026-08-23
- Deciders: Temper core maintainers
- Supersedes: ADR-0056's hydration-only timeout recovery
- Related:
  - ADR-0049: State-entry timeouts and durable scheduler
  - ADR-0158: Durable and observable cross-entity reactions
  - ADR-0159: Task-scoped schema deployment
  - ADR-0177: Single-owner simulation delivery
  - GitHub issue `nikstern/temper#18`
  - Upstream reference `nerdsane/temper#375`
  - `crates/temper-server/src/state/dispatch/state_timeouts.rs`
  - `crates/temper-server/src/trigger/delivery.rs`

## Context

State-entry timeouts are currently owned by process-local Tokio tasks. The
dispatch hook can reconstruct a remaining delay when traffic happens to hydrate
an entity, but creation into a timed initial state and restart without later
traffic have no durable owner. The reconstruction also searches a bounded recent
event window; once the authoritative entry event falls outside that window, the
fallback grants a fresh budget and silently extends the deadline.

Upstream PR `nerdsane/temper#375` demonstrates creation arming and a boot sweep,
but its in-memory sequence fence and bounded-history reconstruction do not meet
this fork's durable-reaction, scoped-schema, and crash-window guarantees. The
deadline must instead be fixed by committed evidence, recover independently of
actor residency, and use one bounded owner across every supported event store.

## Decision

### 1. Co-commit a normalized timeout intent with the clock-reset event

When `Created` enters a timed initial state, a transition enters a timed state,
or an action named by `reset_on` commits in that state, the entity event carries
a private normalized timeout intent. Its immutable identity is derived from the
tenant, durable entity journal identity, committed source sequence, declaration
identity, and exact schema digest. The absolute deadline is computed once as:

`committed event scheduler timestamp + after_seconds`.

The intent snapshots the target action and params, expected state, reset action
set, source sequence, exact dispatch authority, and exact schema identity. A source
commit without its mandatory timeout intent fails closed. State exit does not
need a second atomic write: delivery validates the later authoritative timeout
clock before firing and records the obsolete intent as terminally skipped.

**Why this approach**: the source event is the only atomic boundary that already
contains authoritative state-entry time. A separate timer-table write recreates
the source-commit crash window; reconstructing from a hot actor or recent-event
window can reset the budget.

### 2. Reuse the durable reaction lifecycle as the single delivery owner

A timeout intent is a specialized same-entity durable reaction with a
`not_before` deadline and a state-clock precondition. It uses ADR-0158's bounded
recovery scan, pending/claimed/dispatching/terminal lifecycle, expiring lease,
monotonic fencing token, deterministic retry budget, target receipt, and stable
idempotency key. The recovery supervisor retains the earliest future deadline
and wakes that tenant when it becomes eligible. Duplicate notifications and
concurrent workers converge on one lifecycle journal and one target receipt.

Durable event-store configurations do not also arm the old process-local timer.
An explicitly non-persistent server retains bounded volatile scheduling because
there is no crash-survival contract to provide there.

**Why this approach**: timeout delivery and cross-entity reaction delivery have
the same hard problem—turn one committed event into one later idempotent action
across crashes. Sharing the proven owner avoids a second lease, retry, cursor,
and receipt implementation with subtly different failure semantics.

### 3. Validate the authoritative timeout clock from committed evidence

Each intent records declaration identity, source sequence, absolute deadline,
expected state, reset actions, occurrence ordinal, and schema digest. Before
claiming a due intent, the worker loads authoritative entity state under the
persisted scoped journal pin and audits the bounded event tail after the
clock-setting event. A later entry, exit, or reset makes the old delivery
terminally skipped. The worker also
loads the exact compiled declaration and verifies its identity, delay, target
action, parameters, expected state, reset policy, and schema digest. A migration
is terminally skipped, while an incompatible global schema change is terminally
rejected rather than reinterpreted. Scoped event stores also enforce a
migrated-source fence in the same transaction as target append. PostgreSQL locks
all live source-migration rows in deterministic identity order before the active
scope pointer, matching cutover's job-to-pointer order even when multiple jobs
target one source. If append wins, cutover observes a changed source write
version and must revalidate; if cutover wins, the retired-journal append is
rejected and the delivery is skipped.

Transition and reset clocks persist the exact Cedar authority that established
the clock, matching volatile-timer behavior without inheriting a restart
process's ambient authority. Bootstrap `Created` has no request principal and
therefore uses the explicit `timeout-scheduler` service identity; deployments
must grant that identity only the timeout actions their schemas declare. Cedar
denial is terminal and observable. The synthetic rule has no principal override,
so recovery cannot silently replace either persisted authority.

Every successful timeout target co-commits its source state as private receipt
evidence. Entity replay and snapshots retain a bounded counter per timed state.
New clocks carry the next ordinal, and delivery skips ordinals beyond the exact
declaration's `max_occurrences`. This closes the target-commit/lifecycle-ack
crash window without an unbounded tenant or entity journal scan.

The target dispatch uses the persisted scoped bundle.
Entity snapshots remain ordinary state acceleration and are not a second timer
source of truth.

Events written by the former process-local implementation have no normalized
intent or durable lifecycle to recover. They are not guessed into existence:
deployment must drain those volatile timers before enabling durable ownership.
This intentionally avoids backward compatibility with an unprovable clock.

### 4. Use absolute scheduler time and fail safe on clock anomalies

All persisted timestamps and comparisons use `sim_now()`. A future deadline is
never recomputed from process uptime. A forward jump makes overdue work eligible
immediately. A backward jump leaves the same absolute deadline in storage and
does not fire early. Timestamp overflow, an implausibly future committed source
timestamp, missing clock evidence, or schema disagreement reaches an explicit
terminal/error observation instead of silently extending or firing the timer.

Wall-clock Tokio instants may pace the production supervisor only; they do not
determine persisted ordering or deadline identity.

### 5. Keep storage semantics backend-neutral and observable

The timeout intent lives in the existing persistence envelope, and its lifecycle
uses the existing event-store append/list/read contract. PostgreSQL, Turso,
Redis, and Sim therefore share the same atomic source evidence and optimistic
fence semantics. Backend-parity tests cover the intent envelope and lifecycle;
crash-window tests cover source commit, claim, target commit, and acknowledgement.

Observe exposes delivery kind, stable ID, deadline, schema digest, state, attempt
count, terminal outcome, and sanitized failure reason. Metrics distinguish
queued, overdue, claimed, fired, stale-clock skipped, schema rejected, retried,
lease recovered, and dead-lettered outcomes without entity IDs or other
high-cardinality labels.

## Rollout Plan

1. Ship readers for the additive intent/clock fields and backend-parity tests.
2. Drain process-local timers, then atomically enable co-commit and durable
   delivery while disabling durable-store volatile timer ownership.
3. Deploy with timeout delivery dashboards and compare queued, overdue, fired,
   skipped, rejected, and duplicate-reconciliation counts in Datadog.

## Readiness Gates

- Creation, passivation/hydration, and hard process restart preserve one deadline.
- Every supported event store passes identical envelope and lifecycle tests.
- Crash tests at every lease/receipt boundary produce at most one target event.
- Scoped digest change and migration tests cannot dispatch under the wrong bundle.
- Forward/backward clock and duplicate-wakeup DST tests are deterministic.
- L0-L3, DST review, code review, live E2E, and deployed Datadog verification pass.

## Consequences

### Positive

- Restart and hydration cannot extend a committed deadline.
- Timeout delivery inherits proven bounded ownership, retry, fencing, receipt,
  and operator-observability semantics.
- Scoped entities continue under their exact committed bundle rather than the
  process's current active pointer.

### Negative

- Every timeout clock reset adds private intent bytes to the source event and
  lifecycle writes to the delivery journal.
- Reaction observability now includes a distinct timeout delivery kind and must
  present it clearly rather than implying it was authored as a reaction rule.
- Pre-deployment process-local clocks must be drained because they have no
  durable identity from which recovery can be proven.

### Risks

- A stale-clock check after claim but before target commit could race a state
  change. Delivery binds the observed target sequence into the actor command;
  an intervening reset/exit causes an optimistic-concurrency retry and then a
  stale-clock skip. The actor also enforces idempotency and co-commits the receipt.
- A global hot-swap could remove the target action. Exact schema comparison
  rejects the stale intent; scoped delivery resolves the immutable retired
  bundle by digest.
- Migration cutover could race the final target append after clock validation.
  The append-time migrated-source fence and cutover's source-write-version check
  serialize the two commits without blocking manual successor activation.
- Large legacy journals could exhaust recovery work. Keyset paging and explicit
  event budgets bound each cycle; failure is observable and never extends time.

### DST Compliance

- Identity, ordering, deadlines, lease expiry, and retry selection use
  `sim_now()`, committed sequences, and ordered collections.
- SimStore models source/lifecycle/receipt crash boundaries and duplicate wakes.
- Tokio time is isolated to production supervisor pacing and marked
  `// determinism-ok`; no OS randomness, filesystem, network, or unbounded task
  fanout is introduced in simulation-visible logic.

## Non-Goals

- Application-specific watchdog entities or manual Resume/Retry actions.
- Exactly-once external WASM or webhook side effects beyond the entity-event
  receipt boundary.
- Changing `[[state_timeout]]` authoring syntax.
- Silently treating closed upstream PRs #384 or #393 as dependencies.

## Alternatives Considered

1. **Port upstream PR #375 unchanged** — rejected because its boot sweep uses
   process-local ownership and can grant a fresh budget when bounded history no
   longer contains the entry event.
2. **Add a dedicated timer table and polling worker** — rejected because a
   second non-atomic write recreates the source-event/timer crash window and
   duplicates ADR-0158's delivery machinery.
3. **Append Created/Fired/Cancelled timer events to the source journal** — sound
   only if every cancellation can join the originating action append and every
   timer worker has its own durable lease. The normalized intent plus existing
   lifecycle provides those semantics with less new machinery.
4. **Hydrate every actor at boot** — rejected because it is unbounded, retains
   actors unnecessarily, and still does not provide a durable delivery fence.

## Rollback Policy

Before durable ownership is enabled, the additive fields and readers can remain
while volatile arming stays active. After enablement, rollback must first stop
creating new timeout intents, drain or export every non-terminal timeout
delivery, and only then restore volatile arming. Reverting the worker alone would
strand committed deadlines; enabling both owners would reintroduce duplicate
delivery races.
