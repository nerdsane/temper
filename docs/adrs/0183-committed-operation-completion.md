# ADR-0183: Committed-operation completion

- Status: Proposed
- Date: 2026-10-08
- Deciders: Temper core maintainers (organizational acceptance pending)
- Related:
  - ADR-0182: Idempotency-key request binding
  - ADR-0048: Dispatch retries and actor-side deduplication
  - `crates/temper-server/src/idempotency/`
  - `crates/temper-server/src/state/dispatch/`
  - `crates/temper-server/src/trigger/dispatcher.rs`

## Context

PR #523 currently binds keys and deduplicates effects, but ties ownership to a
request future. Dropping that future releases Pending even when a credentialed
adapter's execution/revocation task survives. A duplicate can therefore start a
second execution. Separately, EffectsApplied does not imply that an awaited
reaction cascade has completed: suppressing replay cascades can lose unfinished
work, while rerunning every historical cascade duplicates completed work. The
HTTP effects-only cache shortcut also predates the new reaction suppression.

The implementation direction below is approved for development by the parent
orchestrator of PR #523. That authorization is not organizational/human acceptance
of this ADR; its status remains Proposed pending maintainer review.

## Decision

### Sub-Decision 1: Server-owned, process-local operation receipts

A bound committed actor/key has one receipt and one owned continuation, not one
job per retry. Once started, caller cancellation detaches only the caller's wait.
Execution ownership survives through the real effects operation, including any
awaited adapter execution and credential cleanup. The continuation retains the
original committed request/context, first integration mode and logical reply.
Live receipts are not evicted by response-cache TTL or cache pressure.

If the keyed actor entry disappears before receipt adoption, the dispatcher
fails closed with a committed-but-completion-unavailable/unknown response. It
starts neither fresh effects nor reactions and does not create an unretained
continuation. The business commit remains. Later verified history replay does
not infer the lost obligation. This pre-receipt gap can leave effects unstarted;
no automatic completion recovery is promised. It is distinct from an already
started worker's Interrupted receipt, which retains its claim and root lease.

Root work uses the existing semaphore-based bounded execution mechanism, with
64 root completion slots per server cache. Active descendant receipts share a
strong root permit lease, while their contexts carry only a weak lease. They
never compete for permits held by waiting ancestors; a root slot is released
only when all tracked owned descendants finish, and an unknown descendant pins
it. Each owned operation gets one continuation, not each observer/retry. Requests rejected or cancelled before a continuation starts do not
justify reporting that a committed action was not applied.

### Sub-Decision 2: Separate effects readiness from reaction termination

The reaction obligation is registered before EffectsReady can be published:

- PendingEffects -> RunningEffects -> EffectsReady;
- PendingReactions -> RunningReactions -> ReactionsTerminal, or explicitly no
  reaction obligation for deliberate core-only writes.

An `await_reactions=false` caller observes EffectsReady while the owned cascade
continues. An independent stronger retry can join the same cascade's terminal
milestone without relaunching it. Top-level dispatch, nested ReactionDispatcher
calls and HTTP cache handling share this protocol. Rate-limit and composite
core-only writes must remain core-only.

The first integration mode is immutable. A first `await_integration=true` request
publishes EffectsReady only after its inline integration/callback and required
credential cleanup. A later false-to-true integration request does not upgrade
an earlier background integration, rerun it, or wait for an untracked milestone.
It receives the first operation's logical response. This limitation is explicit.

Historical replies without a live receipt do not establish new reaction
obligations. Normal best-effort rule failures terminate a cascade; they are not
an instruction to redeliver it. Completed effects must not rerun just to recover
a demonstrably unstarted reaction phase.

### Sub-Decision 3: Phase-aware ancestry and failure

Logically awaited continuations preserve ancestry even when physically spawned.
Genuinely independent timers, spawns, integrations and other detached handoffs
clear inline dependency ancestry while retaining their existing identity/key,
tracing and callback-budget contracts.

An inline callback cannot wait on an ancestor's unfinished effects. A reaction
back-edge can reuse its ancestor's ready effects without launching another
cascade or waiting for its own ancestor's terminal milestone. Independent
callers still await their requested completion milestone.

Two independently owned roots can also discover a reaction cycle after their
effects have both begun (regression seed 51980). Their receipts register exact-
attempt wait edges under one process-local lock. Registration and cycle choice
are atomic; traversal is ordered and bounded by the callback lineage budget,
and live edges are bounded by root capacity times that budget. Unique edge
tickets are removed on observer drop, error or receipt completion, so old
attempts/cancelled observers cannot manufacture a cycle for a later attempt.

Only an internal edge closing a proven all-reaction-phase cycle with ready
effects may reuse its target's EffectsReady. The closing edge is then an
effects-only back-edge, not a reaction-terminal obligation; it neither finishes
the target receipt nor delivers another cascade. Every non-cut dependency and
all remaining per-root rules still run before that root's terminal milestone.
Independent observers still join their own root's terminal. This is not an SCC-
wide shared terminal: a cut peer may have later rules after the closing edge,
and its independent observers continue to wait for those rules. Unfinished-
effects cycles are refused; graph-budget exhaustion never yields cached success
and is conservatively Unknown when a child join cannot be established.

Caller detachment is not worker failure. A known, quiescent failure can make the
affected phase retryable; unknown termination with possibly surviving work must
instead publish an explicit interrupted/unknown failure and prevent concurrent
re-execution. Any takeover must establish quiescence first. Dropping a future or
a join handle does not establish quiescence. The implementation must retain this
distinction rather than relying on a generic release-to-Pending Drop guard.

Actual termination with possibly surviving child work is conservatively
Interrupted/Unknown and non-reclaimable. Its receipt and bounded root permit are
retained for the process lifetime. Waiters whose requested milestone cannot be
established receive an explicit failure; an already-proven EffectsReady remains
usable for effects-only callers. No automatic or per-key operator recovery is
implemented. This deliberately trades availability for bounded safety. A process
restart discards local bookkeeping but does not prove remote quiescence or safe
exactly-once recovery.

Normal returned failures remain retryable when typed evidence establishes that
known server-owned execution, callbacks and required cleanup have completed.
This is a local completion fact, not a claim that no remote side effect occurred.
Native execution timeout, panic, unresolved cleanup or failed joins remain
Unknown unless the relevant work is explicitly drained. WASM receiver/thread
termination must be a typed `WorkerTerminated` error, distinct from ordinary
invocation errors actually delivered by the worker. This requires a narrow
`temper-wasm` error/engine adaptation and exhaustive-consumer review, not string
classification or a general child-task registry. Native adapters likewise return
`CompletionUnknown` when process reaping cannot be established; ordinary returned
errors retain their retryable, locally joined meaning. Timeout, unwind, join and
credential-cleanup boundaries propagate explicit uncertainty through the joined
context rather than inspecting error strings.

## Rollout Plan

1. Add production-boundary red regressions on PR #523's current base, including
   credential mint/revoke cancellation overlap and unfinished reaction joins.
2. Introduce the receipt/continuation boundary and migrate HTTP, nested reactions,
   context ancestry and deliberate core-only callers together.
3. Exercise cancellation, mode upgrades, cycles, pending/terminal distinctions,
   worker termination, capacity and identity. Obtain independent review before
   publication. No deployment is implied by this proposed ADR. The new public WASM error
   variant requires downstream exhaustive matches to handle worker termination
   explicitly; ordinary returned invocation errors keep their current meaning.

## Readiness Gates

- Real credentialed adapter work cannot overlap because a caller disconnected.
- EffectsReady cannot be observed before the reaction obligation is registered.
- Independent reaction waiters cannot observe success before their requested
  cascade milestone; reaction back-edges cannot self-wait.
- Worker failure tests distinguish quiescent recovery from interrupted/unknown
  surviving work, without false success or duplicate execution.
- Existing binding, authorization, tenant, historical/current projection and
  detached-context regressions remain passing.
- Readability/storage guards pass without new allowances or baseline increases.

## Consequences

### Positive

Cancellation no longer changes execution ownership. Completion choices are
explicit across protocol and internal callers. Reaction delivery is no longer
inferred from whether a response happened to be replayed.

### Negative

Live operation receipts consume memory until their tracked phases terminate.
Callers may receive an interrupted/unknown failure instead of an unsafe retry.
Unknown operations permanently consume their root slots within that process;
enough such failures can exhaust completion capacity. No automatic or per-key
operator recovery is included. The first integration mode remains observable and
cannot be upgraded later.

### Risks

Surviving child tasks, cyclic cascades and bounded root capacity require real
runtime proofs. Child quiescence must not be inferred from dropped handles.
Process loss still loses non-durable receipt/reaction progress.

### DST Compliance

Use existing simulated time/IDs, ordered maps and controlled single-thread test
scheduling. Server-owned tasks are the existing runtime's post-commit execution
boundary, not independent simulated implementations. Tests gate real adapter,
store and reaction boundaries and retain failing seeds. No task/thread-local
ownership state or new wall-clock/random input is introduced.

## Non-Goals

Durable reactions, exactly-once external effects, callback-key derivation,
false-to-true integration waiting upgrades, actor/store/OData query redesign,
and unrelated late-commit admission guarantees are outside this decision.

## Alternatives Considered

1. Release ownership whenever a caller is dropped — rejected because child work
   can survive and overlap a new owner.
2. Treat effects readiness as total request completion — rejected because
   awaited reactions can still be pending.
3. Rerun all replay cascades — rejected because completed/historical requests
   must not launch fresh reactions.
4. One semaphore permit and detached task for every retry/descendant — rejected
   because it creates unbounded work or descendant starvation.

## Rollback Policy

Do not roll back to immediate caller-drop takeover as a safety fallback.
If continuation execution cannot be maintained safely, report interrupted/unknown
and stop unsafe redelivery. Any rollback of published behavior requires an
explicit maintainer decision and validation of in-flight operation handling.

## Review corrections (development-authorized, acceptance still pending)

Actor admission and completion capacity are different resources. The former
bounds concurrent `ActorRef::ask` attempts per tenant/type/action and is released
when that actor-admission round trip ends, before observing owned effects or
reactions. Descendants still acquire normal admission; no cap/FIFO bypass is
introduced.

Known in-process WASM `/tdata` calls carry internal execution lineage and shared
completion evidence/capacity to the OData boundary. This context is not a header
and cannot be supplied by external callers. Authentication and authorization
still use the existing authenticated security context. Child action keys remain
independent: the parent's key is not copied to another binding. Dropping an
observed POST or entered audit-dispatch wait marks the parent Unknown, even if
the guest catches the ABI timeout and returns success. A normal handler return
ends that wait observation but does not clear typed descendant uncertainty or
certify universal actor quiescence. The existing first-integration-mode contract
is unchanged; this does not upgrade deliberately background integration or
external HTTP. Read-only GET waits do not acquire this write obligation.

Terminal workflow-root drain is an owned post-effects obligation, scheduled
before entering reactions as before the receipt migration. Finalization keeps
the same workflow identity and terminal-status predicate. Non-owned/historical
and immediate error/capacity responses also finalize applicable terminal roots;
the registry suppresses duplicate drain scheduling. Caller detachment must not
remove the owning continuation's finalization obligation.

PendingReactions recovery is tested using a test-only handoff fault in the real
worker. There is no production await at that boundary; this is injected phase-
failure coverage, not evidence of a naturally interruptible cooperative window.

Typed Unknown takes precedence at tracked owned integration-result boundaries.
Raw native/guest results remain diagnostic evidence, not completion authority.
Known uncertainty suppresses result-derived success/failure callbacks and
implicit compensation. An awaited callback that newly establishes Unknown also
stops subsequent outcome handling; already committed callbacks are not undone.
Ordinary joined results and deliberately background first-mode behavior retain
their existing callbacks. This precedence was explicitly authorized during the
review correction; it is not inferred from guest success or error strings.

### Transitive awaited prechecks and denial audit

Commons Consume prechecks and synchronously awaited GovernanceDecision creation
are descendants, not detached work. They inherit execution evidence, root lease,
ancestry and callback budgets while retaining their service authority, explicit
tenant and independent keys. This applies to both local POST branches and the
synchronous requested-denial audit reachable from local GET. A read-only GET does
not gain a cancellation obligation merely because it is slow; its audit wait is
observed only when that known write is entered. Deliberately detached audit work
elsewhere remains detached.

A returned HTTP response means the handler wait returned. It is not universal
actor/initialization quiescence, proof of non-application, or terminal execution
of a PostgreSQL tell/202 enqueue. Those pre-adoption and existing asynchronous
milestones remain outside this correction. Typed uncertainty from an already
adopted descendant must survive ordinary HTTP errors and denial responses.

### Published-review correction: versioned cold reply authority

The parent authorized this bounded candidate after the operator requested that
PR523 be unblocked. This is development authorization, not an assertion that the
owner explicitly chose a design or that this Proposed ADR is organizationally
accepted. Old cold-retry availability changes deliberately: missing, malformed,
unsupported or unverifiable reply provenance returns IdempotencyKeyUnverifiable
(409), after authorization and binding-mismatch checks.

An actor's committed state digest does not prove a dispatcher's logical reply.
Inline WASM/native callbacks can change that reply; even a pure/background
request can fail required projection, reaction or completion-capacity work after
commit. Therefore all dispatcher entry points are conservatively classified as
requiring post-completion proof. Only truly direct actor replies are
core-authoritative; atomic composite child writes also require post-completion
proof, as explained below. This intentionally refuses cold
retries of dispatcher-origin pure/background actions too. Warm retained
receipts keep their original result and milestones. We do not persist a new
completion/finalization record or claim restart exactly-once/reaction recovery.

New keyed events atomically carry a versioned reply proof alongside their
binding and core-state digest. Its domain-separated hash binds the key, binding,
core digest, reply class and trusted first mode. The first dispatcher mode
includes integration waiting and reaction scope/waiting; it is carried in the
internal actor message and frozen for later pending-receipt adoption. No request
header/parameter supplies this proof. Direct actor callers explicitly request
only the actor reply. Optimistic-concurrency retries stamp the work they actually
commit with the same rules; atomic composite subwrites explicitly record
Composite/PostCompletion rather than certifying their post-append projection.

Recovery checks stored proof, binding and reconstructed core digest, never
today's trigger declarations, current state or independent correlation UUIDs.
Raw optional proof JSON allows legacy and malformed proofs to hydrate normally
then fail closed on key reuse, including snapshots. Core-authoritative history
returns the original prefix with no new custom integrations/reactions/timers/
spawns. The former historical-composite integration exception is withdrawn;
legitimate process-local owned/pending completion is not disabled. Ambiguous old
records cannot retroactively acquire authority from a changed spec, nor from
this new format. A future durable-final-reply design would still need an explicit
legacy policy and is outside this correction.

The compatibility loss also applies to an optimistic-concurrency loser on an
independent server: catching up the winning dispatcher event proves the commit,
not the winning server's final reply. Identical requests without its retained
receipt therefore refuse with `IdempotencyKeyUnverifiable`; different requests
still fail binding comparison first. Same-process retained receipts and warm
responses remain available. The deliberately removed-receipt fault likewise
cannot turn its initial unknown outcome into a later historical success.

The bounded classification also records an explicit `Composite` mode for atomic
composite child/sub-write keys. This is **PostCompletion**, not DirectCore:
`append_batch` proves core commitment, but the mandatory query projection runs
after it and can fail. Even an ordinarily successful composite child key cannot
provide a cold reply without final completion proof. Neither a parent proof nor
another child's stamp proves this child's projection or whole-composite success.
This intentional availability cost avoids inventing an authoritative child-reply
contract. Strict positive cold replies remain possible for truly direct actor
boundaries (including direct composite-parent core calls); no required post-core
work is certified by their core proof.

Known matching requests with unproven logical reply/completion receive an
explicit reply/completion diagnostic, distinct from a key whose original
request cannot be verified. Both retain HTTP 409 `IdempotencyKeyUnverifiable`;
this does not turn a proved binding into proof of a final outcome.
