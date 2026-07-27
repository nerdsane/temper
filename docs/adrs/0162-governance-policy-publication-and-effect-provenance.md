# ADR-0162: Governance policy publication and effect provenance

- Status: Accepted
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Class A authentication edge
  - ADR-0158: Injection escaping
  - `crates/temper-server/src/authz/policy_persistence.rs`
  - `crates/temper-server/src/api/decisions.rs`
  - `crates/temper-server/src/entity_actor/actor.rs`
  - `crates/temper-platform/src/hooks/governance_callback.rs`

## Context

The exhaustive PR #346 security-diff scan validated additional failures around the
otherwise sound Cedar and ClickHouse injection repairs:

1. deleting the final durable policy leaves its old permit active because an
   empty durable snapshot is not published to the Cedar engine;
2. independent server instances can both accept approve and deny for one
   PendingDecision because resolution uses read-check-write plus a process-local
   approval lock;
3. GovernanceDecision callbacks store free-form target strings and later
   substitute `governance-service`, bypassing an explicit target-tenant Cedar
   denial;
4. actor replay treats an empty historical effect receipt as missing and derives
   effects from the current transition table, so an old idempotent retry can
   acquire a later-spec effect without its later guard; and
5. GovernanceDecision becomes terminal before its required policy/callback
   effects have durable completion receipts;
6. one process can activate an older snapshot after a newer one, and replicas
   do not converge before serving authorization decisions;
7. a generic internal caller can invoke GovernanceDecision.Approve with a
   structurally valid but non-durable receipt;
8. local WASM TData calls can supply trust headers and turn the host into a
   cross-tenant administrative deputy; and
9. dispatcher-level idempotency shortcuts return a cached success without
   checking that the key is still bound to the same action and parameters.

These are one protocol-fragmentation problem, not five independent missing
checks. Policy CRUD, REST decision approval, generic GovernanceDecision actions,
callback dispatch, and actor replay each maintain a different partial view of
the same security facts.

## Decision

### Versioned tenant-policy snapshot publication

Policy mutation will operate on one versioned, named tenant-policy snapshot.
The store must conditionally replace the expected version, including with an
empty set. The live Cedar engine is replaced only from the exact committed
snapshot and records the same version. Create, update, toggle, delete, decision
policy installation, compensation, and boot recovery use this primitive.

An empty committed mutable set is authoritative. Storage absence or read
failure is an error and must not be represented as an empty set. Immutable
control-plane authority is registered separately as a configured baseline and
composed into every snapshot activation; mutable CRUD cannot delete or replace
that baseline, and its reserved identity cannot appear in durable rows.

Snapshot activation holds the tenant version guard across engine and cache
replacement, so an older reader cannot downgrade a newer local activation.
Every HTTP authorization ingress and in-process local TData ingress loads the
durable snapshot before authorization; a storage/convergence failure returns
503 rather than serving stale authority. This adds one snapshot read per request
until a store-level invalidation feed can preserve the same invariant.

**Why this approach**: row-level writes plus a later best-effort reload cannot
prove that durable and live policy state agree. A versioned snapshot makes that
invariant explicit and testable across Turso and PostgreSQL.

### One durable decision-resolution owner

PendingDecision resolution will use a store-level expected-state/version
transition shared by approve and deny. The winning record owns the policy
snapshot version, linked GovernanceDecision delivery, and compensation token.
Retries resume that exact owner. Rollback may remove or replace state only when
its ownership/version still matches.

The process-local approval mutex is removed after the durable primitive covers
all paths; it is not retained as a second correctness mechanism.

**Why this approach**: immutable policy insertion prevents text overwrite but
does not choose one terminal outcome. Exactly-one resolution belongs at the
durable boundary shared by every replica.

### Target-minted callback capabilities

Governance callbacks will not obtain authority from free-form tenant/type/id/
action strings. The waiting target creates a typed callback capability bound to:

- source GovernanceDecision id;
- target tenant, entity type, and entity id;
- the allowed approve and deny actions;
- capability version and deterministic expiry; and
- stable delivery id.

Registration and delivery verify the capability. Internal service identity is
transport identity only and cannot widen the capability. Late registration uses
the same proof. The capability is HMAC-authenticated with a domain-separated key
derived from `TEMPER_VAULT_KEY`; callback registration fails closed when a shared
vault key is unavailable. The WASM host mints the token only when callback tenant
and entity id equal the exact invoking actor, and HTTP plus direct-dispatch
boundaries validate it before an actor event can commit.

**Why this approach**: rechecking the registering principal at delivery cannot
model a long-lived suspended workflow reliably. A target-minted capability
preserves the authority that existed when the target chose to wait without
granting arbitrary deputy power.

### Explicit effect delivery progress before terminal exposure

Governance approval exposes terminal `Approved` only after its required policy
receipt and callback delivery are durably complete. `Approving` and `Denying`
are explicit nonterminal actor states, and the durable PendingDecision owner
records policy-publication and GovernanceDecision-dispatch progress. Each
effect has a stable identity and completion receipt; retry resumes the first
incomplete effect and never reruns a successful irreversible prefix.

The REST wrapper and generic bound action must share this generated protocol.
There will not be two approval implementations with different preconditions.

Approve and deny each expose one composite custom effect. Approval verifies the
exact durable PendingDecision owner, its resolution phase and publication
version, and the enabled named `decision:<id>` policy row before callback and
finalization. Treating this as one actor effect prevents a crash from replaying
an already-successful prefix of an uncheckpointed effect list. Once governance
dispatch begins, the approval owner and policy are retained on any ambiguous
failure; a competing deny cannot be admitted by an unsafe rollback.

**Why this approach**: a best-effort post-commit hook can report failure but
cannot undo the already-journaled terminal state or prove exactly-once delivery.

### Local TData identity and idempotency boundaries

WASM-provided tenant, principal, agent, and action-context headers are stripped
before both local and external delegation. Local calls inherit the invoking
security context; only the exact target-owned callback registration path may
inject system/admin transport identity after minting a bound capability. The
PendingDecision-to-GovernanceDecision lookup is a narrow source-tenant store
lookup, not a generic cross-tenant administrative query.

Process-level completed-response shortcuts are removed from dispatcher and
OData layers. Every retry reaches the actor, where the durable outcome binds the
idempotency key to the original action and parameter digest before an optional
response cache is consulted.

### Versioned historical effect receipts

Every committed entity event records whether its effect receipt is present,
the exact custom/scheduled/spawn effect result, and the spec/transition version
that produced it. `present and empty` is distinct from `legacy absent`.

Replay preserves present receipts byte-for-byte. Legacy absence may be migrated
only with the exact historical table/version; otherwise replay keeps the receipt
empty and emits a diagnostic rather than deriving from the current table.
Durable idempotency outcomes cannot change after initial commit.

**Why this approach**: historical behavior is a fact. Current specs may govern
new actions, but they must not rewrite the effects of old actions.

## Rollout Plan

1. **Phase 0 (PR #346)** — add shared types/store contracts, migrate existing
   rows/events conservatively, route every changed policy/decision/callback/
   replay path through the canonical protocol, and delete superseded local
   helpers.
2. **Phase 1 (same PR before push)** — run Turso/PostgreSQL multi-instance,
   restart, fault-injection, and deterministic replay E2E; verify explicit empty
   policy publication and exactly-once callback/effect progress.
3. **Phase 2 (deployment gate)** — migrate production metadata, restart each
   replica, compare durable policy versions to live engine versions, and verify
   WideEvents/Datadog show no divergence before enabling mutation traffic.

## Readiness Gates

- Final-row delete/disable immediately denies in the same process and after
  restart on every policy backend.
- A stale replica converges to the latest durable snapshot before its next
  request, and a delayed older reader cannot downgrade a newer activation.
- Two independent instances racing approve/deny produce exactly one successful
  terminal owner, one matching policy outcome, and one callback outcome.
- A callback without an exact target-minted capability is rejected; tampering
  any bound field is rejected.
- Failure after any effect resumes only the first incomplete effect after
  restart; completed effects are not repeated.
- A direct GovernanceDecision approval without the exact durable resolution
  owner and named policy row fails even if its actor fields and in-memory policy
  appear valid.
- WASM trust headers cannot select another tenant/principal locally or leak a
  callback capability to an external lookalike URL.
- Reusing an idempotency key with another action or parameters fails in the
  actor instead of returning a process-cache hit.
- An effect-free event replayed under a later effect-bearing spec remains
  effect-free and returns the identical idempotent response.
- No touched Rust file exceeds 500 lines; fmt, strict Clippy, full affected
  workspace tests, DST review, and independent code/security review are clean.

## Consequences

### Positive

- One durable source of truth replaces parallel policy and decision semantics.
- Revocation, multi-replica resolution, callback authority, and replay behavior
  become explicit invariants with backend-conformance tests.
- Retry and recovery are observable, deterministic, and exactly resumable.

### Negative

- Policy mutations and decision resolution require an additional versioned
  transaction/readback rather than a fast in-memory-first update.
- Request ingress performs a durable policy snapshot read to guarantee
  cross-replica revocation convergence; this is a deliberate latency/load cost.
- Existing callback registrations and legacy events need conservative migration.
- Store schemas and generated GovernanceDecision behavior change together.

### Risks

- A permissive legacy migration could reintroduce authority. Migration therefore
  fails closed: ambiguous callback authority is disabled, and ambiguous empty
  effect history stays empty.
- Cross-PR work in ARN-170/ARN-192 may touch the same store/actor contracts. The
  implementation must reuse or rebase the canonical primitive rather than copy
  it into another module.

### DST Compliance

- Versions, capability ids, delivery ids, expiry, and retry ordering derive from
  persisted inputs plus `sim_now()`/`sim_uuid()` only.
- Store and actor iteration uses deterministic ordered collections.
- Fault and restart tests cover every state transition and prove idempotent
  recovery without wall-clock sleeps or process-global correctness state.

## Non-Goals

- Redesigning Cedar syntax or ClickHouse binding; the PR's typed injection
  controls already survived the scan.
- Granting new roles or changing application-specific approval policy.
- Preserving unsafe legacy callback authority or current-table effect inference.

## Alternatives Considered

1. **Patch each endpoint** — Rejected because local locks, empty-set special
   cases, target Cedar rechecks, and retry flags leave parallel truth sources.
2. **Keep the process mutex and add a distributed lock** — Rejected because a
   lease is not the durable decision result and complicates crash recovery.
3. **Authorize callbacks as `governance-service` everywhere** — Rejected because
   transport identity is not target authority and enables the confused deputy.
4. **Re-run current guards/effects during replay** — Rejected because it rewrites
   committed history and can both add and drop behavior after spec evolution.

## Rollback Policy

Before enabling mutation traffic, the migration retains the previous durable
rows as read-only audit history. Rollback may restore the prior binary only if no
new-version mutation has committed. After the first versioned policy, decision,
callback, or event receipt is written, rollback requires a forward converter;
silently re-enabling ambiguous legacy behavior is forbidden.
