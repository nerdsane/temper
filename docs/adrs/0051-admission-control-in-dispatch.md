# ADR-0051: Per-Tenant Admission Control in Dispatch

- Status: Proposed
- Date: 2026-04-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0160 (tenant-database-isolation.md): tenant model this ADR reuses
  - ADR-0167 (wasm-default-timeout.md): module-specific gate precedent
  - ADR-0048: Dispatch retry (runs after admission grants)
  - `crates/temper-server/src/state/mod.rs:217` (existing per-tenant entity index)
  - `crates/temper-server/src/state/admission.rs` (new)
  - `crates/temper-server/src/state/dispatch/actions.rs` (enforcement site)

## Context

Temper has no platform-level admission control. Under bursty load — e.g., Katagami's 2026-04-17 `build_session_message` submitting 11 `Session.Configure` dispatches in 38 seconds — every ask is accepted immediately, routed to the entity actor, and the downstream contention produces `MailboxFull` and `AskTimeout` errors.

The only existing concurrency gate is `TEMPER_MONTY_REPL_MAX_CONCURRENCY` in `dispatch/wasm.rs:57-59` — a per-WASM-module semaphore, default 2. It is narrowly scoped: protects the LLM inference path, not the dispatch layer. No analogous primitive exists for "don't admit a 12th Session.Configure for this tenant when 11 are already in flight."

Each app that needs this writes it from scratch. Katagami's `finalize_spawned_session` contains a hand-rolled "drain next Queued regenerate_embodiment job" loop (`finalize_spawned_session/src/lib.rs:474-535`) — a workaround for the missing primitive. It is job-type-specific, not reusable, and only covers one of many bursty paths.

## Decision

Introduce an in-process admission controller in `temper-server` that gates every action dispatch by a per-`(tenant, entity_type, action)` permit. Specs declare caps; the controller enforces them before the ask reaches the actor. FIFO queueing is a contract-level guarantee, not an implementation accident.

### Sub-Decision 1: Platform primitive, not a Temper app

Admission lives in `temper-server` core, owned by `ServerState`. It is not an entity type, not a Temper app, not a separate service.

**Why**: two reasons.

1. **Zero hops on the hot path.** An app-level `ConcurrencyGate` entity would require a loopback HTTP call before every dispatch — adding the exact latency overhead we want to prevent.
2. **Tenant keying exists already.** `entity_index: Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>` at `state/mod.rs:217` keys by `{tenant}:{entity_type}`. Admission controller reuses this tenancy model directly — no new tenant resolution plumbing.

(The alternative was considered and rejected; see Alternatives.)

### Sub-Decision 2: Spec surface

Entity specs declare caps:

```toml
[admission]
max_concurrent_creates = 5                  # applies to Create action
max_concurrent_actions = { "Submit" = 3 }   # per-action overrides
queue_depth = 50                            # pending acquisitions before 503
queue_timeout_seconds = 30                  # max wait per acquirer
```

Defaults when omitted:
- `max_concurrent_creates`: unlimited (backward-compatible)
- `max_concurrent_actions`: empty (no gating on non-Create actions)
- `queue_depth`: 100
- `queue_timeout_seconds`: 30

**Why per-action overrides**: some actions are cheap (Heartbeat) and some are expensive (Configure, Submit). One cap per entity type is too coarse.

### Sub-Decision 3: Enforcement in `dispatch_tenant_action_core`

Flow inside `state/dispatch/actions.rs`:

```
1. Compute key = (tenant, entity_type, action_name).
2. Look up Semaphore for key. If none, no-op pass-through.
3. Acquire a permit with queue_timeout_seconds budget.
   - Success: proceed to actor_ref.ask (ADR-0048 retry wrapper).
   - Timeout: return DispatchError::Deferred { retry_after_ms }.
4. On response (OK or Err), drop permit via RAII guard.
```

HTTP layer maps `DispatchError::Deferred` to 503 with `Retry-After`. The enum variant is already reserved by ADR-0048.

**Why before the ask**: a full mailbox is a symptom of missing admission. Gating earlier prevents the symptom entirely.

### Sub-Decision 4: FIFO as a contract, not an accident

The `AdmissionController` contract promises strict first-in-first-out queueing:

- Implementation backs onto Tokio's fair semaphore (already FIFO).
- A dedicated test interleaves 100 concurrent acquirers with randomized arrival delays and asserts grant order matches arrival order exactly.
- The test runs in CI. Any future change that violates FIFO (e.g., switching to a priority queue) fails the test.

**Why contract-level**: under-the-hood details change. Users of the controller rely on the guarantee. If we ever need priority tiers, they ship as separate named controllers per tier — never reordering inside one controller.

### Sub-Decision 5: Runtime overrides for ops

Admin endpoint `PATCH /_admin/admission/{tenant}/{entity_type}` accepts a JSON body:

```json
{ "max_concurrent_creates": 20, "max_concurrent_actions": { "Submit": 10 } }
```

Applies immediately. Scoped to the target tenant/entity. Existing queue and in-flight permits adjust to the new cap (if shrinking, waits for natural drain — never revokes permits).

**Why**: deploys are slower than incidents. When a customer is saturated at 09:00 on a Tuesday, ops needs to bump their cap in minutes, not in a redeploy cycle.

### Sub-Decision 6: Interaction with ADR-0048 retry

Order: admission first, retry second.

- Admission waits up to `queue_timeout_seconds` for a permit.
- On grant, ADR-0048's `ask_with_backoff` runs under its own total-deadline budget (default 30s).
- Caller's total time = admission wait + retry budget. Both are bounded.

If admission denies (queue timeout), the caller gets `DispatchError::Deferred` immediately — no retry budget is consumed. Callers (or intermediate HTTP proxies) retry the whole call per Retry-After, restarting the admission flow.

## Rollout Plan

1. **Phase 0** — `AdmissionController` ships with unlimited permits for every key. No behavior change.
2. **Phase 1** — First spec declares `[admission]` (Session via ADR-0036). Observe `temper_admission_granted_total`, `_queued_total`, `_deferred_total` for a week.
3. **Phase 2** — Katagami cleanup: delete `submit_next_queued_regeneration`. Rely on admission for fairness.
4. **Phase 3** — Fleet-wide admission declarations for entity types whose `_deferred_total` or `_queued_total` exceeds thresholds under observed load.
5. **Phase 4** — Admin override endpoint behind auth, exposed to SRE.

## Readiness Gates

- FIFO test green in CI.
- `temper_admission_wait_time_ms p99 < 5s` under synthetic 20-concurrent, 5-cap load test.
- `temper_actor_mailbox_full_drop_total` approaches zero on Session entity once Session `[admission]` is active.

## Consequences

### Positive
- Bursty workloads queue with fairness instead of cascading into `MailboxFull`.
- Capacity tuning moves from WASM-code changes to spec edits + live admin overrides.
- App-level rate-limiting hacks (Katagami's dequeue loop) become deletable dead code.

### Negative
- Callers must handle 503 + Retry-After properly. SDKs need audit.
- Debugging "why is my Submit slow?" gains a new layer (admission queue vs. retry delay vs. actor processing). Mitigated by per-layer histograms exposed in Datadog.

### Risks
- **Misconfigured caps cause starvation.** Too-low `max_concurrent_creates` rejects legitimate load. Mitigation: runtime override endpoint and Datadog monitor `[OpenPaw] Deferred Admission Spike`.
- **Permit leak under bug.** An RAII guard that doesn't release on panic leaks permits. Mitigation: permit is tied to a `Drop`-safe `OwnedSemaphorePermit`; unit test exercises panic-in-critical-section recovery.
- **Priority starvation between tenants.** No cross-tenant fairness in v1. Mitigation: per-tenant caps are independent, so one tenant's queue cannot starve another's.

### DST Compliance
- Semaphore acquisition uses simulated scheduling; permit ordering is deterministic under DST.
- FIFO contract test includes a DST variant that replays 100 iterations and asserts identical grant order.

## Non-Goals

- Global rate limiting across tenants (different primitive; future work).
- Priority lanes within one controller (separate controllers per priority if needed).
- Queue introspection from Cedar policies (policies run post-admission).
- Adaptive caps based on downstream latency (future work; v1 is static + admin-override).

## Alternatives Considered

1. **Temper platform app (ConcurrencyGate entity type)** — Rejected. Adds one loopback HTTP call per dispatch. Would put admission control under the exact failure mode it's meant to fix.
2. **Hybrid: app first, fold into core later** — Rejected as two-phase overhead. Core-first is faster end-to-end and avoids migration churn.
3. **Rely on mailbox capacity alone** — Rejected. Mailbox full = error, not queue; TigerStyle intentionally refuses to buffer. Admission is the right place for bounded waiting.
4. **Token bucket / leaky bucket** — Rejected for v1. Concurrency semaphore is the shape that matches "N in flight"; rate shapes (requests per second) are a different problem.

## Rollback Policy

Set all `[admission]` configs to unlimited (or delete the blocks). The controller remains in place but becomes a no-op pass-through. Persistent state untouched. No schema or event-log changes.
