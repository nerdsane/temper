# ADR-0181: Verified bounded collection workflows

- Status: Accepted
- Date: 2026-08-24
- Deciders: Temper core maintainers
- Related:
  - [Fork issue #16](https://github.com/nikstern/temper/issues/16)
  - [Fork issue #40](https://github.com/nikstern/temper/issues/40)
  - ADR-0156: Immutable typed cross-entity reference contracts
  - ADR-0158: Durable and observable cross-entity reactions
  - ADR-0177: Single-owner simulation delivery
  - ADR-0178: Durable state-timeout delivery
  - [Upstream PR #420](https://github.com/nerdsane/temper/pull/420)
  - `crates/temper-spec/src/automaton/`
  - `crates/temper-verify/src/composite/`
  - `crates/temper-server/src/trigger/`

## Context

Temper can durably dispatch one cross-entity reaction, await its descendant
tree, type scalar references, send bounded module-data batches, and recover one
state-entry timeout. None of those primitives represents a finite collection as
one workflow. Applications still seal a roster, retain a cursor, derive child
IDs, track attempts and outcomes, count terminal members, decide a join, and
propagate cancellation themselves. Those parallel protocols are difficult to
verify and diverge across restart.

A collection workflow is not merely a larger trigger. It has immutable input,
bounded scheduling policy, durable member state, aggregate terminal semantics,
and a lifecycle independent of any one delivery attempt. The kernel needs to
own that protocol without learning application-specific import, benchmark,
scoring, or solver behavior.

Fork issues #18 and #24 are complete. This decision consumes their timeout and
implemented single-owner simulation semantics. ADR-0177 still has Proposed
status; this ADR does not change that status or broaden its decision. Upstream
PR #420 is useful for read-guard graph closure, but its stale draft head is not
merged wholesale and its unbounded instance-multiplicity non-goal is
insufficient here.

## Decision

### Sub-Decision 1: Declare one bounded workflow in IOA metadata

An automaton may declare collection workflows at top level:

```toml
[[collection_workflow]]
name = "run_checks"
start_action = "StartChecks"
cancel_action = "CancelChecks"
timeout_action = "ChecksTimedOut"
roster_field = "check_ids"
member_entity = "CheckRun"
member_action = "Start"
member_cancel_action = "Cancel"
max_members = 64
max_concurrency = 8
max_attempts = 5
on_success = "ChecksSucceeded"
on_partial_failure = "ChecksPartiallyFailed"
on_failure = "ChecksFailed"
on_cancelled = "ChecksCancelled"
on_timed_out = "ChecksTimedOutJoined"
```

`roster_field` must be a declared list. Every action and target must exist in
the verified bundle. All three budgets are required positive integers. The v1
normative maxima are `max_members <= 64`, `max_concurrency <= 8`, and
`max_attempts <= 5`; `max_concurrency <= max_members` also holds. Five matches
ADR-0158's durable delivery attempt budget. A declaration above any maximum is
an L0 error, not a value that is silently clamped. Names are unique. L0 assigns
each `(entity_type, action_name)` used by `start_action`, `cancel_action`,
`timeout_action`, `member_action`, `member_cancel_action`, or any of the five
`on_*` fields to exactly one role in exactly one collection declaration. Thus
start/cancel/timeout are pairwise distinct, member/member-cancel are distinct,
all five joins are distinct, and no lifecycle, member, cancel, or join role can
alias or recursively trigger another workflow role in v1.

The start action seals the roster from its committed post-state. Empty,
duplicate, oversized, or non-string member values fail before the source event
is appended. Later writes cannot replace or mutate a roster while its workflow
is non-terminal. A new workflow may start only after the prior workflow reaches
a terminal classification. Its atomic start fences any unresolved prior join as
`Skipped` with reason `SupersededByNewWorkflow` and co-commits the prior
`join_status = DeliveryFailed` before replacing the active workflow ID. A stale
join target commit always requires its originating workflow ID still to be the
active ID, so it cannot mutate the source after a later start.

**Why this approach**: top-level metadata makes the workflow visible to every
verification level and keeps orchestration out of application fields. A narrow
v1 avoids embedding an arbitrary map language in trigger configuration.

### Sub-Decision 2: Derive immutable workflow, member, and child identities

The workflow ID is a domain-separated SHA-256 digest of tenant, source entity
type and ID, workflow declaration name, source action, committed source
sequence, and schema digest. Roster order is canonical input: the sealed roster
is preserved in declaration order, while duplicate member values are rejected.

Each member ID is derived from the workflow ID, zero-based roster index, and
member value. The child entity ID is the member ID. Derivation is pure, shared,
versioned, and covered by golden vectors. No UUID, clock, database sequence, or
delivery order contributes to identity.

The member action declares exactly these kernel-owned parameters:

- `workflow_id`, `member_id`, `member_value`, and `source_entity_id`: `string`;
- `member_index`: `int`.

Those names are reserved for collection targets. V1 member actions cannot
declare additional parameters because the workflow has no application payload
mapping. The action must be valid from the target automaton's initial state.

Member delivery is create-if-missing. For an absent child, one atomic target
append materializes the normal spec initial state, applies `member_action`, and
records the collection delivery receipt. A retry finding that exact receipt is
an idempotent success. An existing child without that exact workflow/member
receipt is an identity collision and is permanently `Rejected`; collection
delivery never adopts or overwrites it. V1 member entity types cannot declare an
ADR-0156 `entity_id = true` key, because the collection's domain-separated ID
would compete with the key's canonical hash. Other typed-reference and
prospective-state contracts from ADR-0156 still apply to the first append.

**Why this approach**: a retry, duplicate wakeup, or restart must address the
same child without consulting mutable application state.

### Sub-Decision 3: Persist one workflow journal and one delivery per member

The start transition uses one store-level atomic multi-journal append to commit
the source event and normalized collection-start intent, create the private
`_CollectionWorkflow` lifecycle journal, and retain the active workflow ID in
source snapshot/replay metadata. The lifecycle contains the declaration/schema
identity, sealed roster, next undispatched index, configured budgets, counts,
requested outcome, terminal classification, original authority, and source pin.
It is created directly in `Running`; atomic creation leaves no durable workflow
`Pending` state. If any backend cannot commit that batch, the source action fails
closed.

The declared cancel and timeout transitions use the same atomic primitive to
commit the source event and versioned collection-control intent and move the
exact workflow journal to `Cancelling` or `TimingOut` under an expected workflow
sequence. The control intent records the requested outcome, source sequence,
control authority, and source schema pin. The batch also increments a durable
control epoch, fences every admitted non-receipted member delivery, and creates
at most eight cancel deliveries for receipted members that are still `InFlight`.
Admission compares the expected workflow sequence and control epoch. A target
commit re-reads the current workflow sequence, retries an unrelated optimistic
sequence conflict, and requires the admission's control epoch, member identity,
and `InFlight` delivery ID to remain current. Thus either the target receipt
commits before control and its cancel intent is co-committed, or control commits
first and the stale target commit cannot land. An action response cannot precede
the complete start/control batch.

Recovery discovers the redundant start and control intents through the existing
bounded source-journal scan and reconciles all named journals before admitting
work. It never reconstructs a lifecycle state absent from the atomic evidence.
Thus neither crash recovery nor an already-running owner can admit a member
across a committed cancel or timeout.

Each admission is one atomic append that moves the member from `Pending` to
`InFlight`, advances the roster cursor, and creates a normal fenced durable
delivery with collection context naming its workflow and member identity. The
append checks the workflow sequence, control epoch, and available concurrency
window; a crash cannot materialize only one side or over-admit on recovery. The
existing reaction owner retains claim, lease, retry, receipt, descendant, and
crash-reconciliation semantics. A target commit records its receipt under the
workflow fence but does not imply member success while awaited descendants are
outstanding. The later append that terminalizes the durable delivery and applies
its matching member outcome is one fenced multi-journal append. Duplicate
completion cannot increment an aggregate twice.

Workflow lifecycle is:

`Running -> {Cancelling, TimingOut} -> {Cancelled, TimedOut}`,

with `Running -> {Succeeded, PartiallyFailed, Failed}` for ordinary aggregation.
`Cancelling` and `TimingOut` are durable quiescing states, not terminal states.
They retain the first fenced control request while active children converge.

Member lifecycle is:

`Pending -> InFlight -> {Succeeded, Failed, Cancelled, TimedOut}`.

The journal stores the bounded per-member terminal outcome, attempts, delivery
status, and sanitized failure class. It does not depend on an application
cursor, counter, or projection to recover truth. Lifecycle append ordering uses
optimistic sequence fencing. Ambiguous writes are reconciled from journal
evidence before retry.

**Why this approach**: reusing the durable delivery owner preserves one crash,
lease, and receipt protocol. The workflow journal adds the missing aggregate
truth without creating backend-specific tables or polling state.

### Sub-Decision 4: Admit only an explicit deterministic concurrency window

At most `max_concurrency` non-terminal members may be `InFlight`. Admission
walks the sealed roster in index order and examines at most eight entries per
workflow turn, the v1 admission-scan budget. It emits at most the available
window. Exhausting that budget with undispatched entries persists the exact
cursor, records `admission_budget_yielded`, and schedules another supervisor
turn; it is observable non-quiescence, not success or failure. A terminal member
completion atomically advances workflow state and makes another bounded
admission pass eligible. Sequence fencing, rather than process-local exclusion,
is authoritative when owners compete.

Production recovery uses the existing tenant recovery supervisor and keyset
paging. One tenant turn examines at most 100 total source, workflow, member,
delivery, or descendant-lineage records. Every scan consumes that shared budget.
Exhaustion persists the opaque keyset cursor for the current record kind, emits
`recovery_budget_yielded`, and schedules the next turn without reporting
quiescence. Simulation drains the same logical work in workflow-ID, member-index
order. No member creates a Tokio task, polling loop, or unbounded collection in
the actor path.

Collection declarations cannot set `drop_ok`. Delivery outcomes map to members
without application policy:

- `Succeeded` with a matching target receipt becomes member `Succeeded`;
- `Skipped` and permanent `Rejected` become member `Failed`;
- `DroppedAllowed`, which v1 never authors, is defensively read as member
  `Failed` with reason `UnsupportedDropAllowed`;
- transient `DeadLettered`, or reaching the declaration's `max_attempts`, becomes
  member `Failed` with an exhausted-attempt reason; and
- `Pending`, `Claimed`, and `Dispatching` remain member `Pending`/`InFlight` and
  are reconciled through their existing lease and receipt rules.

During the atomic control append, undispatched members become `Cancelled` or
`TimedOut`. Every admitted original delivery without a target receipt becomes
the standard terminal status `Skipped` with stable reason
`CollectionControlBeforeTargetCommit`, and the same append changes its member to
the requested `Cancelled` or `TimedOut` outcome. A worker holding an older claim
cannot commit after that fence.

Collection context and the workflow control epoch are inherited by every
descendant delivery. After control commits, no uncommitted descendant target
event may pass the epoch fence. Recovery keyset-pages the bounded lineage,
terminalizes those deliveries as `Skipped` with reason
`CollectionControlBeforeDescendantCommit`, and keeps the member `InFlight` until
the lineage and member-cancel delivery quiesce. Descendant target events already
committed before control are durable and are not undone; the member cancel action
is the application's compensation boundary. L2 verifies that no descendant
target event commits after the control epoch.

A child with a committed member receipt receives one durable
`member_cancel_action` delivery under the control request's authority and the
workflow's persisted source schema pin. The action declares the five member
parameters above plus `requested_outcome: string`, whose only accepted values
are `Cancelled` and `TimedOut`; no other parameters are allowed in v1. The
original start authority is used only for member work and the terminal join,
never for later human control.

Cancel-delivery outcomes map exhaustively:

- `Succeeded` with a matching cancel receipt becomes the requested member
  `Cancelled` or `TimedOut` outcome;
- `Skipped` becomes that requested outcome only when receipt evidence proves the
  same cancel already committed; every other cancel-delivery `Skipped` becomes
  member `Failed`. Original deliveries fenced before child creation are handled
  atomically by the separate control rule above;
- permanent `Rejected` and transient `DeadLettered` become member `Failed` with
  a cancellation failure class;
- `DroppedAllowed`, which v1 never authors, becomes member `Failed` with reason
  `UnsupportedDropAllowed`; and
- `Pending`, `Claimed`, and `Dispatching` remain `InFlight` until receipt,
  fencing, retry exhaustion, or another terminal mapping is durably appended.

Manual delivery retry is forbidden for collection member and cancel deliveries;
it cannot reopen a terminal workflow. An operator must start a new source
transition and therefore obtains a new workflow identity.

### Sub-Decision 5: Join exactly once from durable member outcomes

When every sealed member is terminal, the kernel computes one classification:

- all members succeeded: `Succeeded`;
- at least one succeeded and at least one failed: `PartiallyFailed`;
- all non-cancelled members failed and none succeeded: `Failed`;
- a durable cancellation request moved the workflow to `Cancelling`: `Cancelled`
  after every member is terminal, even if some cancel deliveries failed;
- a durable timeout request moved the workflow to `TimingOut`: `TimedOut` after
  every member is terminal, even if some cancel deliveries failed.

The matching `on_*` action is represented by exactly one durable delivery to the
source entity with the original start authority and scoped schema pin. Each of
the five join actions declares exactly `workflow_id: string` plus
`total_members: int`, `succeeded_members: int`, `failed_members: int`,
`cancelled_members: int`, and `timed_out_members: int`; no other parameters are
allowed in v1. The source action remains the application's only state transition;
the kernel does not set application statuses directly.

The first fenced control request wins and fixes `requested_outcome`; a later
cancel/timeout control intent becomes an idempotent no-op with an observable
reason. Ordinary success/failure aggregation wins only if it commits before a
control request. Terminal classification and the single join-delivery intent are
written together after all members are terminal. The workflow separately stores
`join_status` as `Pending`, `InFlight`, `Delivered`, or `DeliveryFailed`.
`Succeeded`, or `Skipped` backed by the matching source receipt, becomes
`Delivered`; any other `Skipped`, permanent `Rejected`, or terminal
`DeadLettered` becomes `DeliveryFailed`. `DroppedAllowed`, which v1 never
authors, also becomes `DeliveryFailed` with reason `UnsupportedDropAllowed`.
The append that terminalizes the join delivery co-commits its `join_status`
under the workflow sequence fence. Delivery and receipt evidence are
authoritative after an ambiguous write; recovery deterministically derives and
repairs `join_status` before returning Observe detail or attempting retry.
Classification never changes because delivery failed. Only an underlying
transient `DeadLettered` delivery may be manually retried under ADR-0158 with the
same delivery ID and receipt. `Skipped` and `Rejected` joins are not retryable.
The retry changes only `join_status`, so it cannot append a second source event.
Late or duplicate member receipts remain observable but cannot change the
classification or create a second join intent.

### Sub-Decision 6: Cancellation and timeout share one propagation path

The declared `cancel_action` co-commits its control intent, which moves the
workflow to `Cancelling`, prevents further admission, marks pending members
cancelled, and dispatches the declared member cancel action to in-flight children
through durable deliveries. Successful children remain successful. The workflow
joins as `Cancelled` only after every member is terminal, including an explicit
failed outcome for exhausted cancellation delivery.

The declared `timeout_action` must be the `on_timeout` action of exactly one
existing `[[state_timeout]]` on the source automaton. Its `reset_on` set must
contain exactly `start_action`; no other action may reset the clock while this
declaration exists. `start_action` must enter or remain in the timeout's state,
and the post-state of every `on_*` join action must be outside that state. The
atomic collection start therefore co-commits a fresh ADR-0178 timeout intent,
and the workflow journal stores its exact intent ID, clock-setting source
sequence, deadline, and schema digest.

While that workflow is `Running`, every source action other than its declared
`cancel_action` and `timeout_action` whose `from` set includes the timed state
must preserve that state. L0 rejects a declaration whose transition graph could
exit and later re-enter the state under another action while the workflow is
active, and runtime repeats the active-workflow check before commit. Cancellation
or timeout may leave the state because the workflow simultaneously leaves
`Running`; no later clock can replace the workflow's bound timeout intent.

Timeout delivery checks that the active workflow is still `Running`, has no
`requested_outcome`, and names that exact intent before applying
`timeout_action`. No active workflow, a quiescing or terminal workflow, a later
workflow ID, or a different clock makes the stale timeout terminally `Skipped`
with reason `StaleCollectionClock`; it commits neither the application action nor
collection control. A matching timeout action co-commits its control intent,
moves the workflow to `TimingOut`, and invokes the common propagation path while
using `TimedOut` for undispatched or successfully cancelled members. The
collection primitive creates no second deadline, timer table, or wall-clock
scheduler. Restart cannot refresh the deadline because ADR-0178 binds it to the
stored clock-setting event.

### Sub-Decision 7: Verify a finite collection model and full dependency closure

L0 validates syntax, symbol references, reserved parameters, uniqueness, and
the normative v1 budgets. L1 uses an exact symmetry quotient, not a smaller
`verification_member_bound`: the sealed roster size is chosen from one through
the declaration's `max_members`; members with the same lifecycle, attempt
bucket, receipt class, and control epoch are indistinguishable and represented
by exact integer counts whose sum is that chosen size. The model retains the
next roster index and active window. Because every transition affects one
equivalence class and guards inspect only those counts, the cursor, the workflow
sequence, and the control epoch, quotient bisimulation is a verification
requirement and is tested by exhaustive labelled-versus-quotient comparisons
for rosters of one through eight. L1 also checks declaration boundary shapes at
roster sizes 1, `max_concurrency`, `max_concurrency + 1`, and `max_members`,
including the v1 maxima of eight concurrent and 64 total members.

The model checks:

- a roster is sealed once and never mutates while active;
- active members never exceed `max_concurrency`;
- each roster index maps to one member/child identity;
- duplicate delivery does not duplicate a member outcome or join;
- a superseded join cannot mutate the source after a later workflow starts;
- cancellation and timeout admit no new work;
- a running workflow cannot lose or replace its bound timeout clock through
  source-state exit or re-entry;
- retries never exceed `max_attempts`;
- every terminal classification agrees with the member partition; and
- restart reconstructs an equivalent workflow state.

L2 injects delay, duplication, partial failure, cancellation, timeout, crash,
restart, append ambiguity, budget exhaustion, and callback rejection through the
single-owner scheduler. Each scenario receives 2,048 event applications and
4,096 logical ticks; exhaustion fails with `BudgetExhausted` and can never count
as quiescence or a passing trace. Crash or ambiguity is injected on both sides of
the atomic admission, control, target-receipt, member-outcome, join, and
join-versus-new-start appends. The oracle checks classification and `join_status`,
including rejected, dead-lettered, and superseded joins. L3 runs at least 1,000
generated cases spanning rejected empty rosters, singleton,
concurrency-boundary, maximum-roster, maximum-attempt, and attempted timed-state
exit/re-entry shapes.

Composite seed and verification-plan closure follow both entity-trigger edges
and `cross_entity_state` read-guard edges needed by the source, member, cancel,
and join actions. Compatible graph-closure behavior is ported from upstream PR
#420 after reconciliation with ADR-0156; its stale CLI and unrelated API changes
are excluded.

### Sub-Decision 8: Observe bounded progress without exposing roster payloads

Observe adds these tenant-scoped JSON reads:

- `GET /observe/collection-workflows?limit={n}&cursor={opaque}&status={status}`;
- `GET /observe/collection-workflows/{workflow_id}`; and
- `GET /observe/collection-workflows/{workflow_id}/members?limit={n}&cursor={opaque}`.

List defaults to 50 and caps at 100 workflows. Member pages default to and cap at
64, the v1 `max_members`. Both use opaque keyset cursors and return `next_cursor`
only when more durable rows exist. A list request scans at most 400 tenant-scoped
rows while applying Cedar filters; if that scan budget is consumed first, it
returns the authorized rows found so far plus `next_cursor`. Invalid limits,
cursors, or status values return `400`; unauthorized detail/member reads return
`403`; list rows denied by Cedar are omitted; missing or cross-tenant IDs return
the same `404`; bounded store failures return `503` with a stable sanitized error
category. Reads never hydrate application actors or trigger recovery work.

Every route performs Cedar authorization as
`Temper::Action::"ViewCollectionWorkflow"` against a tenant-scoped workflow
resource containing only workflow ID, declaration, source entity type, and
status. List filtering occurs after the tenant storage boundary and before
response materialization; authorization cannot widen the tenant scope.

Summary fields include workflow ID, declaration, source identity, schema digest,
status, requested outcome, `join_status`, configured budgets, sealed member
count, lifecycle counts, total attempts, oldest active age, and sanitized
terminal or join-delivery reason. Detail returns the same summary plus bounded
member IDs, indices, statuses, attempts, delivery classes, and sanitized failure
classes. Raw member values, roster payload, parameters, authority, security
claims, receipts, and private source fields are never returned.

Metrics record starts, active windows, terminal classifications, member
outcomes, retries, recovered leases, duplicate receipts, join latency, and queue
age. Entity IDs, workflow IDs, member values, and roster sizes are not metric
labels.

### Sub-Decision 9: Keep persistence additive and backend-neutral

Collection intent and lifecycle payloads are versioned additive JSON in the
existing event-store envelope. Postgres, Turso, Redis-backed deployments, and
Sim implement the same append/list/read and optimistic-fence contract; Redis is
not the sole durable record. Parity tests execute one shared semantic suite.

Readers ignore unknown future additive fields but reject an unsupported intent
version. Existing events have no collection intent and remain unchanged. There
is no compatibility shim that infers a sealed roster from legacy application
cursors or counters.

## Rollout Plan

1. Land additive readers plus an internal-only declaration model, validation,
   transition metadata, golden identity vectors, and failing verification cases.
   Public submission returns stable `CollectionWorkflowNotEnabled`; production
   transition tables cannot activate the declaration.
2. Add the versioned workflow/member persistence model and shared store parity
   suite while writers and recovery remain disabled outside tests.
3. Integrate bounded admission, durable member dispatch, joins, cancellation,
   timeout propagation, and restart recovery behind the same capability gate.
4. Add governed Observe surfaces, metrics, and a generic reference app, then run
   L0-L3, randomized DST, full workspace tests, local live E2E, and deployed
   Datadog validation with the gate still closed.
5. Enable public authoring only in the final activation change after every
   readiness gate below is evidenced on every supported backend. The operational
   capability moves from `Disabled` to `Enabled`: starts, controls, writers,
   recovery, and Observe become available together; no release may expose
   partially functional syntax.

Rollback uses a third operational state, `Draining`. `Enabled -> Draining`
rejects new start actions with `409 CollectionWorkflowDraining` but preserves
control actions, delivery, recovery, joins, and Observe until every workflow is
terminal and every join is delivered, exported, or explicitly acknowledged as
failed. Only then may `Draining -> Disabled` stop writers/recovery and hide the
public routes. Additive parsers and readers may remain installed in all three
states.

## Readiness Gates

- The start event and normalized workflow intent are atomic on every backend.
- Crash tests cover start commit, workflow materialization, admission, target
  commit, member accounting, terminal join, and acknowledgement boundaries.
- Duplicate delivery and wakeups never create duplicate children or joins.
- Cancellation and durable timeout prevent new admission and converge under
  bounded retries.
- Join rejection and dead-letter tests retain immutable classification and
  observable, idempotently retryable delivery status.
- Public authoring remains rejected until declaration, verification,
  persistence, execution, control, aggregation, recovery, authorization,
  observability, and rollback tests all pass together.
- Store parity, L0-L3, DST review, code review, live E2E, and deployed Datadog
  verification are clean.

## Consequences

### Positive

- Applications can express bounded map/join behavior entirely through entity
  transitions and declarative metadata.
- Restart-safe progress and terminal truth become kernel-observable.
- One delivery owner supplies fencing, retries, receipts, and recovery for both
  ordinary reactions and collection members.

### Negative

- A source start event adds one normalized collection intent, one workflow
  lifecycle, and up to one durable member delivery per roster entry.
- V1 target parameters and join classifications are intentionally fixed rather
  than offering arbitrary payload mapping or reduction code.
- Roster order is semantically significant because it determines admission and
  child identity.

### Risks

- A source action could race cancellation or a member completion. Workflow
  sequence fencing and first-writer terminal classification serialize the race.
- Large rosters could amplify storage and recovery work. Static `max_members`,
  per-turn admission budgets, keyset recovery, and bounded Observe pages cap it.
- A join action could be removed by schema change. Persisted scoped pins and
  exact declaration identity reject reinterpretation and retain the failure as
  terminal evidence.

### DST Compliance

- Identity, roster order, admission order, attempt selection, joins, timeout
  evidence, and recovery use committed sequences, `sim_now()`, and ordered
  collections.
- Simulation-visible code adds no wall clock, OS randomness, filesystem,
  network, thread, or unbounded spawn.
- Fault cases consume the normative event, tick, member, scan, and attempt
  budgets defined above. Simulation exhaustion is a failing trace; production
  admission/recovery exhaustion persists a cursor and remains non-quiescent.
- No determinism suppression annotation is planned.

## Non-Goals

- ARC import, benchmark, scoring, solver, or domain-specific aggregation.
- Arbitrary map functions, reducers, WASM callbacks, or external exactly-once
  effects.
- Mutable rosters, dynamic member discovery, nested workflows, or unbounded
  streaming collections in v1.
- A new timer subsystem or application-maintained Resume/Retry action.
- Merging the stale upstream PR #420 head or its unrelated CLI/API changes.

## Alternatives Considered

1. **Application cursor and counters** — rejected because every application
   reimplements restart, duplicate, cancellation, and terminal invariants.
2. **One ordinary trigger per roster entry** — rejected because triggers do not
   seal a dynamic roster or own a concurrency window and aggregate join.
3. **WASM loop using the typed batch API** — rejected because module batches are
   invocation-scoped, not durable workflow state, and cannot prove restart.
4. **Dedicated workflow database tables** — rejected because they would require
   parallel atomicity and parity machinery; versioned journals already provide
   the necessary contract.
5. **A second timeout scheduler** — rejected because ADR-0178 already owns
   durable state-entry deadlines and duplicate prevention.

## Rollback Policy

Before collection admission is enabled, additive readers and metadata may remain
deployed. After enablement, rollback must enter `Draining`, reject new starts,
drain or export every non-terminal workflow and member delivery while recovery
and Observe remain active, and only then enter `Disabled` and remove writers.
Reverting the worker while leaving intent creation enabled would strand
workflows; running two owners would violate the concurrency and join invariants.
