# ADR-0182: Bind idempotency keys to the action and canonical request

- Status: Accepted
- Date: 2026-10-06
- Deciders: Temper core maintainers
- Related:
  - ADR-0048: Dispatch retry and error taxonomy (sub-decision 5, actor-side dedup)
  - ADR-0142: Dispatch acknowledges after projection
  - nerdsane/temper#519 finding V6
  - `crates/temper-server/src/idempotency.rs`
  - `crates/temper-server/src/entity_actor/{actor.rs,mod.rs,types.rs}` (`mod idempotency_replay`)
  - `crates/temper-server/src/state/dispatch/{mod.rs,actions.rs}`
  - `crates/temper-server/src/odata/bindings.rs`

## Context

ADR-0048 added `Idempotency-Key` deduplication at three layers: the HTTP
cache in `odata/bindings.rs`, the shared actor-side `IdempotencyCache`, and
the durable `processed_idempotency_keys` set rebuilt from
`EntityEvent::idempotency_key` on replay. Every layer keyed only on
`(tenant:type:id, key)`. Issue #519 V6 showed the consequence. After
`AddItem` with key K, `CancelOrder` (or `AddItem` with another body) with K
returned 200 with the cached `AddItem` response and did nothing. Two
concurrent requests sharing K with different bodies both got 200, and the
losing request was silently dropped. After a restart, the durable path
returned the *current* state, not the original response.

## Decision

### Sub-Decision 1: Canonical request binding

`idempotency::request_binding(action, params)` is a SHA-256 over:

- the tag `temper.idempotency.v1`
- the unqualified action name (`Temper.Example.AddItem` → `AddItem`, the
  same resolution the OData path parser and `resolve_authenticated_params`
  use)
- the canonical JSON of the params:
  - **top-level** server-derived keys removed (`is_server_derived_field_name`,
    the exact predicate `sanitize_action_params` applies before journaling)
  - nested objects hashed with sorted keys
  - arrays in order
  - type-tagged, length-prefixed scalars

Nested keys of any name, including `@odata.*`, are business data and are
bound. Headers, trace and session context, intent, observation metadata,
query parameters and `expected_authorization_precondition` are never inputs.
Both the HTTP layer and the actor compute the binding over params *after*
`resolve_authenticated_params`, so they agree.

### Sub-Decision 2: Persistence

`EntityEvent` gains `idempotency_binding: Option<String>`
(`serde(default, skip_serializing_if = "Option::is_none")`). The actor sets
it next to `idempotency_key` on the live and the ADR-0046 retry path. Cache
entries store the binding too. `EntityState` and the snapshot format are
unchanged, so response shapes and old snapshots stay compatible. The
journal is the source of truth.

### Sub-Decision 3: Lookup semantics

For a request with key K and binding B:

1. **Cache (HTTP and actor).** An entry with binding B is a hit and returns
   the cached original response. Any other binding is a **mismatch**. The
   HTTP lookup reports a mismatch even before effects are marked applied.
2. **Durable path** (K is in `processed_idempotency_keys`):
   - If the latest in-memory event carries K, verify it and reply with the
     current state. That state is the original response.
   - Otherwise replay the journal from sequence 0 with
     `ReplayPolicy::StrictFullJournal`, stopping right after the first event
     carrying K (`ReplayTarget::stop_after_idempotency_key`). Verify that
     event and reply with the replayed state, which is the original response.
3. **Verification.**
   - A bound event compares bindings.
   - A **legacy** event (written before this ADR) is verified by repeating
     the transform the actor applied when journaling: `event.action` must
     equal the unqualified action, and `event.params` must equal
     `sanitize_action_params(normalize_ref_action_params(params))`. This is
     exact, so legacy history is neither trusted blindly nor rejected.
4. **Fail closed.** If the key is recorded but its event cannot be found
   (for example, a snapshot lists a key with no journal event), the journal
   cannot be read, strict replay validation fails, or there is no journal,
   the request is rejected as **unverifiable**.
5. Only successful requests bind a key. Failed attempts leave the key free.

### Sub-Decision 4: Errors

The actor replies `success: false` with one of two constants:
`IDEMPOTENCY_KEY_MISMATCH` or `IDEMPOTENCY_KEY_UNVERIFIABLE`.
`dispatch_tenant_action_core` maps them to
`DispatchError::IdempotencyKeyMismatch` or
`DispatchError::IdempotencyKeyUnverifiable` **before** post-dispatch
effects, so no trajectory, effect, reaction or projection runs. OData
returns:

| Case | Status | Code |
|---|---|---|
| Same key, different action or body | **422** | `IdempotencyKeyMismatch` (IETF Idempotency-Key draft) |
| Key recorded, original request cannot be verified | **409** | `IdempotencyKeyUnverifiable` |

A string marker is used instead of a typed `EntityResponse` field because
that struct is constructed at hundreds of sites. The constants are exact
values defined in one module.

### Sub-Decision 5: The "original logical response"

A replay returns the entity state right after the key's event. Byte-level
differences that are not request semantics are allowed:

- top-level `@odata.*` annotations
- `events_since_snapshot` and `last_snapshot_sequence_nr`, which depend on
  when snapshots happened
- the per-event `idempotency_binding` hash on legacy history

## Consequences

### Positive
- Reusing a key can no longer silently drop a different request. Callers get
  a documented 422.
- Retries after a restart return the original response, even after later
  actions.
- Legacy journals keep working, and unverifiable keys fail closed.

### Negative
- A cold duplicate whose key is not on the latest event replays the journal
  from sequence 0 (PostgreSQL keeps only the latest snapshot). It is bounded
  by `MAX_EVENTS_SINCE_SNAPSHOT`, and a replay that exceeds the budget fails
  closed with 409.
- Each event now carries a 64-hex-character binding.

### Risks
- Internal flows that reuse a caller's key for a *different* action on the
  *same* entity (inherited trigger and callback keys) now get an explicit
  422 where they were previously swallowed. The full suite is green. Any
  such flow found later should get a distinct key, not a weaker check.
- A successful duplicate runs post-dispatch effects only when its dispatcher
  claims a fresh commit whose effects never ran (dropped reply or failed
  effects). Every other duplicate is a replay without transition effects
  (review correction 1).
- Cold retries of keys committed before this ADR carry no result provenance.
  They fail closed with 409 instead of returning a possibly re-derived 200
  (review correction 2). A mismatched request is still 422.
- The binding and the result digest add 128 hex characters per keyed event.

### DST Compliance
- Hashing is pure. Canonicalization sorts keys explicitly, independent of
  map iteration order. No clocks, randomness or I/O were added outside the
  existing replay path.

## Non-Goals

- The bounded cache window (1,000 keys per actor, 1 h TTL) and eviction of
  durable keys (#519 V7).
- `--actor-runtime postgres` actor-backed types, which do not use
  idempotency keys (HTTP 202 enqueue).
- Returning the original value after transition-rule drift. Without stored
  per-event state the server cannot recompute it, so drift fails closed
  with 409.

## Alternatives Considered

1. **Store the binding in `processed_idempotency_keys`.** Changes the
   snapshot and response format, and still cannot return the original
   response. Rejected.
2. **Compare raw request bytes.** Breaks on key reordering and transport
   differences. Rejected.
3. **Treat unbound legacy keys as success.** Preserves V6 for every existing
   journal. Rejected in favour of exact reconstruction plus fail-closed
   handling.

## Rollback Policy

Reverting the code is safe: older builds ignore the extra
`idempotency_binding` payload field (`serde(default)`) and fall back to
key-only dedup.

## Review corrections (nerdsane/temper#523)

The PR review raised three findings. Each is reproduced by a new failing
regression test (`entity_actor/actor_test.rs`, module
`review_523_regressions`) before any production change. The original four
frozen `factory_regression` files are not touched.

### Correction 1: a replayed response is not a newly committed transition

Finding r4203860027: a cold retry sent the historical state through
`run_post_dispatch_effects`. That cancelled or armed state timeouts,
broadcast a stale `state_change` and enqueued projection writes.

Decision: each idempotency cache entry records who owns the post-dispatch
effects of the commit behind it:

| State | Meaning |
|---|---|
| `Pending` | Written by the actor for a fresh commit. Effects have not been claimed. |
| `Claimed` | One dispatcher owns the effects. |
| `Applied` | The effects completed. |
| `Historical` | A response rebuilt from the journal or the latest in-memory event, not a new commit. |

`dispatch_tenant_action_core` calls
`IdempotencyCache::claim_post_dispatch_effects`. Only the claimant runs the
full pipeline, and it marks the entry `Applied` on success or releases it
back to `Pending` on failure. This keeps the existing recovery: a retry
after a dropped actor reply or a failed integration still runs the effects
exactly once.

Every other reply is a replay. It returns the original response and runs
no transition effects: no state timers, no broadcast, no projection, no
webhooks, no spawns, no scheduled actions. One exception keeps intended
behaviour: a `Historical` composite duplicate still re-runs its re-emitted
trigger integrations, which are idempotent by design. The ownership state
is explicit; no unrelated response flag is reused.

### Correction 2: immutable execution provenance

Finding r4203860035: replaying the journal with the *current* transition
table made a spec change (for example `+= 1` to `+= 2`) return a different
value with 200.

Decision: every keyed event also stores `idempotency_result`, a SHA-256 over
the post-commit logical state (`status`, `item_count`, `counters`,
`booleans`, `lists`, `fields`). This is 64 hex characters per event, not a
snapshot and not a response history.

A duplicate returns 200 only if the state rebuilt for it (the latest live
state, or the journal replayed up to the key's event) hashes to the stored
value. If the table drifted, the reply is 409 `IdempotencyKeyUnverifiable`.
Events written before this ADR have no provenance and also fail closed with
409 when the request matches. A mismatched request is still 422 because the
binding check runs first. The temper server never returns a 200 computed
under different rules.

### Correction 3: racing writers recheck the key after catch-up

Finding r4203860041: two server instances with independent caches
received the same key. The losing actor hit an optimistic-concurrency
conflict, replayed the winner's event, then re-ran its own request. That
appended a second event with the same key.

Decision: in the ADR-0046 retry loop, immediately after catch-up replay,
the actor re-checks `processed_idempotency_keys`. If the key is now
present, it resolves it exactly like the durable duplicate path:

- same logical request: the original response, as a `Historical` replay
- different request: 422
- unverifiable: 409

The losing request is never re-executed.

### Scope

All changes stay inside the existing 20-file footprint:

- `idempotency.rs`: ownership states
- `entity_actor/{mod.rs,actor.rs,types.rs}`: provenance and the retry recheck
- `state/dispatch/actions.rs`: claim gating
- `state/dispatch/composite.rs`: provenance on atomic sub-write events
- the regression tests

`state/dispatch/effects.rs` is not edited. The integrations-only replay
path in `actions.rs` mirrors step 5 of `run_post_dispatch_effects`.

### Correction evidence

Pre-fix production code: `23a207b9296e96c7deb0efd116873cf13cfab604`. At that
point the only working-tree changes were the new tests and this plan.

The four original frozen `factory_regression` files are byte-identical to
`refs/factory/regression-tests` (`4bba5d9`); `git diff --stat` against that
ref prints nothing.

The new tests live in `entity_actor/actor_test.rs`, module
`review_523_regressions`. They run through real `ServerState` dispatch and
post-dispatch effects. Review 3 uses two independent server instances, each
with its own pool, actor system and caches, sharing a private
**PostgreSQL 16** testcontainer.

```bash
cargo test -p temper-server --lib review_523 -- --test-threads=1            # red, then green
cargo test --locked -p temper-server --release --lib review_523 -- --test-threads=1
```

**Red on the pre-fix code: 0 passed, 6 failed.** Each failure is the
reviewed defect:

```text
review_523_1_cold_replay_of_superseded_key_runs_no_transition_effects  FAILED
  REVIEW-523-1 (cold=true): replaying the historical Running response armed a timeout on a Stopped entity
  left: [("TimedTask", 1)]   (right: [])
review_523_1_warm_replay_of_superseded_key_runs_no_transition_effects  FAILED
  REVIEW-523-1 (cold=false): ... left: [("TimedTask", 2)]   (right: [("TimedTask", 1)])
review_523_2_counter_effect_change_does_not_alter_original_response    FAILED
  REVIEW-523-2: a 200 replay after a spec update must return the ORIGINAL result ...  "starts": 2 (original 1)
review_523_2_boolean_effect_change_does_not_alter_original_response    FAILED
  REVIEW-523-2: ... "flagged": false (original true)
review_523_3_racing_different_requests_one_wins_other_422             FAILED
  REVIEW-523-3: exactly one journal event may carry the key   left: 2  right: 1
review_523_3_racing_identical_requests_return_original_once            FAILED
  REVIEW-523-3: exactly one journal event may carry the key   left: 2  right: 1
```

Two setup assumptions were corrected before the red pass was recorded. A
cancelled state timer stays counted until its sleeping task wakes, so the
test compares the timer count before and after the retry. The race test
checks the journal before checking the broadcast. Neither change loosened
what the tests detect.

**Green after the fix: 6/6 in debug and release.** In both drift cases the
replay fails closed with 409 `IdempotencyKeyUnverifiable`; the server never
returns a re-derived 200.

The original frozen suite still passes 20/20 in debug and release:
`cargo test --locked -p temper-server --features observe [--release] --test factory_regression`.

| Validation (after the fix) | Result |
|---|---|
| `cargo test --locked -p temper-server --lib idempotency::` | 11 passed (adds claim and historical-replay unit tests) |
| `cargo test --locked -p temper-server --lib entity_actor::` | 108 passed |
| `cargo test --locked -p temper-server --lib state::dispatch::` | 130 passed |
| `cargo test --locked -p temper-server --lib odata::` | 106 passed |
| `cargo test --locked -p temper-server --test dispatch_retry_idempotency` | 1 passed (dropped-reply effect recovery preserved) |
| DST core / platform-boot / platform-consistency / platform-random smoke | 26 / 9 / 13 / 7 passed |
| `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` | ok |
| `cargo nextest run --workspace --no-fail-fast -E 'not test(dst_)'` (historical migration fixtures present) | **3557 passed, 0 failed**, 65 skipped |
| observe-gated `spec_validate_endpoint` / `observe::` / `api::repl::`, `cargo test --doc --workspace` | 2 / 92 / 2 passed, doctests ok |
| readability ratchet, storage dispatch boundary | ok (GT1000 24/24, ALLOW_CLIPPY 36/36) |

**Real HTTP, PostgreSQL 16 behind Toxiproxy.** The Appendix B driver passed
49/49 checks in both debug and release; the earlier phases are unchanged.
The new phases:

```text
== review 3: two `temper serve` instances on the same private database
B-hydrate-1                 200
A-K6-winner                 200
B-K6-different-body         422 IdempotencyKeyMismatch   K6 events: 1
B-hydrate-2                 200
A-K7-winner                 200
B-K7-identical              200 == original response    K7 events: 1
instance B log: "persist hit optimistic-concurrency violation; entering ADR-0046 retry" actual_seq=7 (K7)
== review 1: cold retry of a superseded key
K8-cancel                   200 Cancelled
(restart)
cold-K1-after-cancel        200 == original Draft response
GET status                  Cancelled
catalog row                 Cancelled
journal                     no new event
== review 2: restart under a spec copy with AddItem `items += 2`
drift-K2-retry              409 IdempotencyKeyUnverifiable
journal                     no new event
drift spec live             6 AddItem events replay to 12 items
```

K6 was rejected by the durable-path check: instance B's actor had already
caught up while waiting out the verification gate. K7 went through the
ADR-0046 catch-up re-check. The deterministic unit test pins that path
regardless.

**Limitations:**

- PostgreSQL has no query-plane projection store (that lives in Turso), so
  "no losing projection update" on PostgreSQL is shown by the journal plus
  the absence of a broadcast or timer. The catalog is checked over HTTP.
- In the unit tests the review-1 timer observable is the spawned timer
  count; cancellation is generation-based.
- Keys committed before this ADR fail closed with 409 on a matching cold
  retry (no provenance).
- The spec-drift case fails closed with 409 and does not return the
  original value.

## PR packaging note

The factory caps a PR at 20 files, frozen regression tests included. To stay
within that cap, the durable-path resolver lives as an inline
`mod idempotency_replay` in `entity_actor/mod.rs` instead of its own file. The
complete proof report and the executable end-to-end driver are reproduced
verbatim below as Appendix A and Appendix B, not as separate `.proofs/` files.
Local copies stay in the sandbox: `.proofs/519-01/` (excluded only through
`.git/info/exclude`) and `/tmp/fx519/proofs-preserved/`, which also holds raw
histories, journals and server logs. No production, test or policy file is
excluded.

## Appendix A: Proof report (519-01, nerdsane/temper#519 V6)

Evidence recorded 2026-10-06 in the factory sandbox.

Evidence recorded 2026-10-06 in the factory sandbox. Design: ADR-0182
(`docs/adrs/0182-idempotency-key-request-binding.md`).

### Upstream check

`gh` is not installed in the sandbox. I used the unauthenticated GitHub REST API
instead:

- `GET /repos/nerdsane/temper/issues/519`: open. V6 reads "Idempotency-Key not
  bound to action/body … expected 422/mismatch error".
- `search/issues?q=repo:gabrik/temper+idempotency+is:pr`: 0 results.
- `search/issues?q=repo:nerdsane/temper+idempotency+is:pr`: no PR addresses
  V6 (#276, #385, #396, #402 cover other idempotency topics).

### Prerequisites (private, sandbox-only)

The sandbox had no Docker daemon running, so I started a private one on
`/tmp/fx519` (data root, exec root and socket all under that path). PostgreSQL
and Toxiproxy share a user-defined network. No shared or factory database was
touched.

```text
docker server: 29.8.0
network: fx519-1791303095-net
pg container: fx519-1791303095-pg          (postgres:16)
toxiproxy container: fx519-1791303095-toxi (ghcr.io/shopify/toxiproxy:2.9.0)
postgres server_version: 16.15 (Debian 16.15-1.pgdg13+2)
toxiproxy api 127.0.0.1:32768 version: {"version": "2.9.0"}
proxy create: {"name":"pg","listen":"[::]:25432","upstream":"fx519-1791303095-pg:5432","enabled":true,...}
select via proxy (in-network client): 1
```

The commands are the plan's Stage 0 block, run with
`DOCKER_HOST=unix:///tmp/fx519/docker.sock`. A latency toxic was added and
removed before the tests were written, which confirmed fault injection works.

### Red, then green (frozen tests at `refs/factory/regression-tests` = `4bba5d9`)

```bash
cargo test --locked -p temper-server --features observe --test factory_regression -- --nocapture
cargo test --locked -p temper-server --features observe --release --test factory_regression -- --nocapture
```

**Unfixed code**: 7 passed, 13 failed. All 13 failures are
`FACTORY_REGRESSION_ASSERTION` panics. Every one of them received **200**
where the contract requires 422, 409, or the original response:

```text
FAILED concurrent_different_bodies_one_wins
FAILED different_action_same_key_is_rejected_{warm,cold}
FAILED different_body_same_key_is_rejected_{warm,cold}
FAILED nested_array_order_is_significant_{warm,cold}
FAILED nested_odata_named_business_property_is_bound_{warm,cold}
FAILED legacy_journal_and_snapshot_same_request_returns_original
FAILED legacy_journal_different_body_is_rejected
FAILED legacy_snapshot_key_without_event_fails_closed
FAILED same_logical_request_returns_original_response_after_intervening_action_cold
ok     binding_respects_tenant_and_auth_{warm,cold}, concurrent_identical_requests_append_one_event,
       failed_request_never_binds_key_{warm,cold}, offline_fold_matches_history_catalog_and_get,
       same_logical_request_returns_original_response_after_intervening_action_warm
```

**Fixed code**: `test result: ok. 20 passed; 0 failed` in both debug and
release.

`git diff --stat refs/factory/regression-tests -- crates/temper-server/tests/factory_regression.rs crates/temper-server/tests/factory_regression/`
prints nothing, so the frozen test tree is unchanged.

### Validation

| Command | Result |
|---|---|
| `cargo fmt --check` | ok |
| `cargo check --workspace` | ok |
| `cargo clippy --workspace --all-targets -- -D warnings` | ok |
| `cargo nextest run --workspace --no-fail-fast -E 'not test(dst_)'` (GEPA wasm modules built; testcontainers on the private Docker daemon; reviewer fetched historical commits `53e2304f…`, `c7cf6a24…`, `ad06abd2…` without moving HEAD) | **3549 passed, 0 failed, 65 skipped**. The earlier run's two `migration_differential` failures came from those commits missing in the sandbox checkout, not from the code. |
| `cargo test -p temper-server --features observe --test spec_validate_endpoint` | 2 passed |
| `cargo test -p temper-server --features observe --lib observe::` | 92 passed |
| `cargo test -p temper-server --features observe --lib api::repl::` | 2 passed |
| `cargo test --doc --workspace` | ok |
| `cargo test --locked -p temper-server --lib idempotency::` | 9 passed (new canonicalization unit tests included) |
| `cargo test --locked -p temper-server --lib entity_actor::` | 102 passed |
| `cargo test --locked -p temper-server --lib state::dispatch::` | 130 passed |
| `cargo test --locked -p temper-server --lib odata::` | 106 passed |
| `cargo test --locked -p temper-server --test dispatch_retry_idempotency` | 1 passed |
| `cargo nextest run -p temper-server --test dst_concurrency_retry --test dst_hotswap --test dst_lifecycle --test dst_multi_tenant --test dst_persistence` | ok |
| `cargo nextest run -p temper-server --test dst_platform_boot` | 9 passed |
| `cargo nextest run -p temper-server --test dst_platform_cedar --test dst_platform_index --test dst_platform_rollback` | 13 passed |
| `TEMPER_DST_RANDOM_MODE=smoke cargo nextest run -p temper-server --test dst_platform_random` | 7 passed |
| `bash scripts/readability-ratchet.sh check .ci/readability-baseline.env` | ok (GT500 79/85, ALLOW_CLIPPY 36/36) |
| `bash scripts/check-storage-dispatch-boundary.sh` | ok |

### Real HTTP flow (Appendix B driver, debug and release)

**Backend**: `temper serve --storage postgres` with the **default legacy actor
runtime**. This path runs EntityActor, the PostgreSQL `events` table and the
`IdempotencyCache`, which is the code changed here.
`--actor-runtime postgres` was not used because it ignores idempotency keys.

**Auth**: real bearer middleware, no test bypass. The ecommerce app runs in
tenant `ecommerce`. That tenant's only supported credential path is a trusted
JWT issuer configured through `TEMPER_TRUSTED_ISSUER_{URL,JWKS,AUD}`. The
script generates a P-256 key, registers it through those variables, and signs
ES256 tokens for `e2e-customer` and `e2e-intruder`.

The reference Cedar policies grant `AddItem` only to `Admin`, a principal kind
a JWT cannot produce. To let the test customer act, the run uses a **private
copy** of `reference-apps/ecommerce/specs` plus one proof-only policy,
`policies/zz_e2e_proof.cedar`:
`permit(principal is Customer, action, resource is Order) when { principal.id == "e2e-customer" };`.
Repository policies are unchanged. The intruder still gets 403.

```bash
dockerd --data-root /tmp/fx519/docker --exec-root /tmp/fx519/daemon-run -H unix:///tmp/fx519/docker.sock &
# Stage 0 block above: network, postgres:16, toxiproxy, proxy "pg" -> $PG:5432
mkdir -p /tmp/fx519/ecommerce-specs && cp -r reference-apps/ecommerce/specs/. /tmp/fx519/ecommerce-specs/
#   + policies/zz_e2e_proof.cedar (above)
cargo build --locked -p temper-cli && cargo build --locked --release -p temper-cli
export DOCKER_HOST=unix:///tmp/fx519/docker.sock FX_RID=<rid> FX_SPECS=/tmp/fx519/ecommerce-specs
python3 e2e.py   # Appendix B debug   target/debug/temper   /tmp/fx519/e2e-debug     # exit 0, 33/33 PASS
python3 e2e.py   # Appendix B release target/release/temper /tmp/fx519/e2e-release   # exit 0, 33/33 PASS
```

The two profiles produced identical response history:

```text
unauthenticated              401
wrong-token                  401
create-order                 201
intruder-add                 403   key=K1  AuthorizationDenied
K1-original                  200   key=K1  items=1
K2-intervening               200   key=K2  items=2
K1-equivalent-retry          200   key=K1  items=1   (Temper.AddItem, reordered keys, +Id, traceparent/session/observe headers)
K1-different-body            422   key=K1  IdempotencyKeyMismatch
K1-different-action          422   key=K1  IdempotencyKeyMismatch   (CancelOrder)
K3-under-latency             -     key=K3  client timed out at 1.5s (Toxiproxy latency 4000ms on PostgreSQL)
K3-retry                     200   key=K3  items=3
K4-under-reset               409   key=K4  ActionFailed "persistence failed: ... EOF" (Toxiproxy reset_peer)
K4-retry-0                   200   key=K4  items=4   (failed attempt did not bind the key)
--- server restart (cold caches, durable path; first write waits out 423 re-verification) ---
cold-K1-equivalent-retry     200   key=K1  items=1   == original K1 response (logical body)
cold-K1-different-body       422   key=K1  IdempotencyKeyMismatch
cold-K1-different-action     422   key=K1  IdempotencyKeyMismatch
cold-intruder-K1             403   key=K1  AuthorizationDenied (no cached body)
cold-K3-retry                200   key=K3  items=3
get-order                    200           items=4
catalog                      200
```

PostgreSQL journal, from
`select sequence_nr,event_type,payload from events where tenant='ecommerce' and entity_type='Order' and entity_id='e2e-<profile>-order'`:

```text
 1 Created  key=-   binding=-
 2 AddItem  key=K1  binding=2c63c0b26c800dfe…  params={"Meta":{"gift":{"note":"n","wrap":true},"tags":["a","b"]},"ProductId":"p-1","Quantity":1}
 3 AddItem  key=K2  binding=d45569aaa393fdad…  params={"ProductId":"p-2","Quantity":1}
 4 AddItem  key=K3  binding=870f93f7cb5d36f4…  params={"ProductId":"p-3","Quantity":1}
 5 AddItem  key=K4  binding=ceb05f2ec472cef4…  params={"ProductId":"p-4","Quantity":1}
```

The bindings are identical in debug and release, so the canonical hash is
deterministic. Checks the script asserted in both profiles:

- Each key appears in exactly one event.
- No `CancelOrder` event exists.
- The sequence is contiguous from 1.
- Mismatches and cold retries appended nothing.

Offline fold of the journal: `status=Draft, items=4`. That matches GET
(`status`, `counters.items`) and the catalog row. The prefix fold through K1
gives `(Draft, 1)`, which matches the original K1 response.

### Omissions

- I couldn't comment on or verify upstream PRs with authenticated `gh`
  (not installed). The read-only REST checks are listed above.
- The ecommerce Cedar policy has no rule that lets a non-Admin principal call
  `AddItem`. The HTTP proof therefore uses a proof-only policy on a private
  copy of the specs, as described above.
- Out of scope per ADR-0182: the bounded cache window and key eviction
  (#519 V7), and `--actor-runtime postgres` actor-backed types.

### Re-check after packaging

The resolver moved into `entity_actor/mod.rs` with no change in behaviour.
After the move:

- the frozen regression command passes 20/20
- `cargo fmt --check` passes
- `cargo clippy -p temper-server --all-targets --features observe -- -D warnings` passes
- the full workspace nextest above passes 3549/3549

## Appendix B: End-to-end driver (`e2e.py`)

Save as `e2e.py` and run it as shown in Appendix A. Set `FX_SPECS` to the
private spec copy with the proof-only policy. Set `FX_SPECS_DRIFT` to the
same copy with `AddItem` changed to `effect = ["items += 2"]`, which the
review-2 phase uses.

```python
#!/usr/bin/env python3
"""Real-HTTP end-to-end proof for nerdsane/temper#519 V6 (ADR-0182).

Usage: DOCKER_HOST=... FX_RID=<rid> e2e.py <profile> <temper-binary> <out-dir>

Boots `temper serve --storage postgres` (default legacy actor runtime) against
a PRIVATE PostgreSQL 16 reached through a PRIVATE Toxiproxy, drives the
ecommerce reference app over HTTP, injects faults, restarts the server, and
compares response history, the PostgreSQL event journal, the OData catalog and
an offline replay fold. Exits non-zero if any invariant fails.
"""
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request

PROFILE, BIN, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
RID = os.environ["FX_RID"]
PG, TOXI = f"{RID}-pg", f"{RID}-toxi"
DB = f"temper_e2e_{PROFILE}"
PORT = {"debug": 39181, "release": 39182}[PROFILE]
BASE = f"http://127.0.0.1:{PORT}"
TENANT = "ecommerce"
ISSUER = "e2e-issuer"
AUDIENCE = "temper"
SPECS = os.environ.get("FX_SPECS", "/tmp/fx519/ecommerce-specs")  # private copy + proof-only Cedar policy
SPECS_DRIFT = os.environ.get("FX_SPECS_DRIFT", "/tmp/fx519/ecommerce-specs-drift")  # AddItem: items += 2
API_KEY = secrets.token_hex(24)
ORDER = f"e2e-{PROFILE}-order"
os.makedirs(OUT, exist_ok=True)

history = []
failures = []


def b64url(data):
    import base64
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def der_to_raw(sig):
    # ECDSA DER SEQUENCE{INTEGER r, INTEGER s} -> 64-byte r||s
    assert sig[0] == 0x30
    i = 2 if sig[1] < 0x80 else 2 + (sig[1] & 0x7F)
    out = b""
    for _ in range(2):
        assert sig[i] == 0x02
        n = sig[i + 1]
        out += sig[i + 2:i + 2 + n].lstrip(b"\x00").rjust(32, b"\x00")
        i += 2 + n
    return out


KEY = f"{OUT}/issuer-key.pem"
subprocess.run(["openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", KEY],
               check=True)
PUB = subprocess.run(["openssl", "ec", "-in", KEY, "-pubout", "-outform", "DER"], check=True,
                     capture_output=True).stdout[-64:]
JWKS = json.dumps({"keys": [{"kty": "EC", "crv": "P-256", "kid": "e2e",
                             "x": b64url(PUB[:32]), "y": b64url(PUB[32:])}]})


def mint(sub):
    header = b64url(json.dumps({"alg": "ES256", "typ": "JWT", "kid": "e2e"}).encode())
    claims = b64url(json.dumps({"iss": ISSUER, "aud": AUDIENCE, "sub": sub,
                                "exp": int(time.time()) + 7200}).encode())
    signing_input = f"{header}.{claims}".encode()
    der = subprocess.run(["openssl", "dgst", "-sha256", "-sign", KEY], input=signing_input,
                         check=True, capture_output=True).stdout
    return f"{header}.{claims}.{b64url(der_to_raw(der))}"


TOKEN = mint("e2e-customer")
INTRUDER_TOKEN = mint("e2e-intruder")


def sh(*args, check=True):
    return subprocess.run(args, check=check, capture_output=True, text=True).stdout.strip()


def docker_port(port):
    return sh("docker", "port", TOXI, str(port)).splitlines()[0]


TOXI_API = docker_port(8474)
PG_PROXY = docker_port(25432)


def psql(sql, db=DB):
    return sh("docker", "exec", PG, "psql", "-U", "postgres", "-d", db, "-tAc", sql)


def toxic_add(name, kind, attributes, stream="downstream"):
    body = json.dumps({"name": name, "type": kind, "stream": stream, "attributes": attributes})
    req = urllib.request.Request(
        f"http://{TOXI_API}/proxies/pg/toxics", data=body.encode(), method="POST",
        headers={"Content-Type": "application/json"})
    urllib.request.urlopen(req).read()


def toxic_remove(name):
    req = urllib.request.Request(f"http://{TOXI_API}/proxies/pg/toxics/{name}", method="DELETE")
    urllib.request.urlopen(req).read()


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg, flush=True)
    if not cond:
        failures.append(msg)


def call(label, method, path, body=None, key=None, token=TOKEN, headers=None, timeout=60, base=None):
    hdrs = {"Content-Type": "application/json", "X-Tenant-Id": TENANT}
    if token is not None:
        hdrs["Authorization"] = f"Bearer {token}"
    if key is not None:
        hdrs["Idempotency-Key"] = key
    hdrs.update(headers or {})
    data = body.encode() if isinstance(body, str) else None
    req = urllib.request.Request((base or BASE) + path, data=data, method=method, headers=hdrs)
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status, raw = resp.status, resp.read()
    except urllib.error.HTTPError as err:
        status, raw = err.code, err.read()
    except Exception as err:  # client-side timeout / reset
        status, raw = None, str(err).encode()
    try:
        parsed = json.loads(raw) if raw else None
    except ValueError:
        parsed = raw.decode(errors="replace")
    entry = {"label": label, "method": method, "path": path, "idempotency_key": key,
             "request_body": body, "status": status, "body": parsed,
             "elapsed_ms": int((time.time() - started) * 1000)}
    history.append(entry)
    print(f"  {label}: {method} {path} key={key} -> {status}", flush=True)
    return status, parsed


def logical(body):
    body = json.loads(json.dumps(body))
    if isinstance(body, dict):
        for k in [k for k in body if k.startswith("@odata.")]:
            del body[k]
        body.pop("events_since_snapshot", None)
        body.pop("last_snapshot_sequence_nr", None)
        for ev in body.get("events", []) or []:
            if isinstance(ev, dict):
                ev.pop("idempotency_binding", None)
    return body


def error_code(body):
    return body.get("error", {}).get("code") if isinstance(body, dict) else None


server = None
server_runs = 0


def start_instance(name, port, specs):
    """Boot one `temper serve` instance on the shared private database."""
    global server_runs
    server_runs += 1
    env = dict(os.environ, HOME=f"{OUT}/home-{name}", TEMPER_API_KEY=API_KEY,
               TEMPER_TRUSTED_ISSUER_URL=ISSUER, TEMPER_TRUSTED_ISSUER_JWKS=JWKS,
               TEMPER_TRUSTED_ISSUER_AUD=AUDIENCE,
               DATABASE_URL=f"postgresql://postgres:fx@{PG_PROXY}/{DB}", RUST_LOG="warn")
    log_path = f"{OUT}/server-{server_runs}-{name}.log"
    log = open(log_path, "w")
    proc = subprocess.Popen(
        [BIN, "serve", "--storage", "postgres", "--app", f"{TENANT}={specs}",
         "--port", str(port), "--no-observe"],
        env=env, stdout=log, stderr=subprocess.STDOUT, cwd="/work/repo")
    base = f"http://127.0.0.1:{port}"
    for _ in range(300):
        try:
            if urllib.request.urlopen(base + "/healthz", timeout=2).status == 200:
                return proc, base, log_path
        except Exception:
            pass
        if proc.poll() is not None:
            raise SystemExit(f"setup: server exited early, see {log_path}")
        time.sleep(1)
    raise SystemExit("setup: server did not become healthy")


def start_server(specs=None):
    global server
    server, _, _ = start_instance("a", PORT, specs or SPECS)


def stop_proc(proc):
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()


def until_verified(label, method, path, body=None, key=None, base=None, headers=None):
    """Retry while the gate answers 423 (background spec verification); 423 writes nothing."""
    for _ in range(240):
        s, b = call(label, method, path, body, key=key, base=base, headers=headers)
        if s != 423:
            return s, b
        history.pop()
        time.sleep(1)
    return s, b


def stop_server():
    stop_proc(server)


def journal():
    rows = psql(
        "select coalesce(json_agg(json_build_object('sequence_nr',sequence_nr,'event_type',event_type,"
        "'payload',payload) order by sequence_nr),'[]') from events where tenant='ecommerce' "
        f"and entity_type='Order' and entity_id='{ORDER}'")
    return json.loads(rows or "[]")


def key_events(events, key):
    return [e for e in events if e["payload"].get("idempotency_key") == key]


ORIGINAL = '{"ProductId":"p-1","Quantity":1,"Meta":{"gift":{"wrap":true,"note":"n"},"tags":["a","b"]}}'
EQUIVALENT = ('{"Meta":{"tags":["a","b"],"gift":{"note":"n","wrap":true}},"Quantity":1,'
              f'"ProductId":"p-1","Id":"{ORDER}"}}')
SECOND = '{"ProductId":"p-2","Quantity":1}'
OTHER_BODY = '{"ProductId":"p-1","Quantity":2,"Meta":{"gift":{"wrap":true,"note":"n"},"tags":["a","b"]}}'
TRANSPORT = {"traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
             "x-session-id": "retry-session", "x-temper-observe-meta-producer.attempt": "2"}
ADD = f"/tdata/Orders('{ORDER}')/Temper.Example.AddItem"
ADD_SHORT = f"/tdata/Orders('{ORDER}')/Temper.AddItem"
CANCEL = f"/tdata/Orders('{ORDER}')/Temper.Example.CancelOrder"


def main():
    psql(f"drop database if exists {DB}", db="postgres")
    psql(f"create database {DB}", db="postgres")
    start_server()

    print("== fail-closed auth", flush=True)
    s, _ = call("unauthenticated", "POST", "/tdata/Orders", json.dumps({"Id": ORDER}), token=None)
    check(s == 401, f"no bearer token is rejected with 401 (got {s})")
    s, _ = call("wrong-token", "POST", "/tdata/Orders", json.dumps({"Id": ORDER}), token="not-a-key")
    check(s == 401, f"wrong bearer token is rejected with 401 (got {s})")

    print("== create order (verification gate + Cedar)", flush=True)
    status = None
    for _ in range(180):
        status, body = call("create-order", "POST", "/tdata/Orders", json.dumps({"Id": ORDER}))
        if status != 423:
            break
        history.pop()
        time.sleep(1)
    check(status in (200, 201), f"order created (got {status}: {body})")

    s, b = call("intruder-add", "POST", ADD, ORIGINAL, key="K1", token=INTRUDER_TOKEN)
    check(s == 403, f"authenticated but unpermitted principal is denied with 403 (got {s})")

    print("== V6 flow (warm)", flush=True)
    s, original = call("K1-original", "POST", ADD, ORIGINAL, key="K1")
    check(s == 200, f"K1 AddItem succeeds (got {s})")
    s, _ = call("K2-intervening", "POST", ADD, SECOND, key="K2")
    check(s == 200, f"K2 AddItem succeeds (got {s})")
    s, b = call("K1-equivalent-retry", "POST", ADD_SHORT, EQUIVALENT, key="K1", headers=TRANSPORT)
    check(s == 200 and logical(b) == logical(original),
          f"warm: equivalent K1 retry returns the ORIGINAL response (status {s})")
    s, b = call("K1-different-body", "POST", ADD, OTHER_BODY, key="K1")
    check(s == 422 and error_code(b) == "IdempotencyKeyMismatch",
          f"warm: K1 with a different body -> 422 IdempotencyKeyMismatch (got {s} {error_code(b)})")
    s, b = call("K1-different-action", "POST", CANCEL, '{"Reason":"changed my mind"}', key="K1")
    check(s == 422 and error_code(b) == "IdempotencyKeyMismatch",
          f"warm: K1 with a different action -> 422 IdempotencyKeyMismatch (got {s} {error_code(b)})")

    print("== fault: latency toxic (client gives up, then retries K3)", flush=True)
    toxic_add("lat", "latency", {"latency": 4000})
    s, _ = call("K3-under-latency", "POST", ADD, '{"ProductId":"p-3","Quantity":1}', key="K3",
                timeout=1.5)
    toxic_remove("lat")
    time.sleep(12)
    s, b = call("K3-retry", "POST", ADD, '{"ProductId":"p-3","Quantity":1}', key="K3")
    check(s == 200, f"K3 retry after lost reply succeeds (got {s} {error_code(b)})")
    check(len(key_events(journal(), "K3")) == 1, "K3 produced exactly one journal event")

    print("== fault: reset_peer toxic during append, then retry K4", flush=True)
    toxic_add("reset", "reset_peer", {"timeout": 0})
    s, _ = call("K4-under-reset", "POST", ADD, '{"ProductId":"p-4","Quantity":1}', key="K4",
                timeout=20)
    toxic_remove("reset")
    s = None
    for attempt in range(10):
        s, b = call(f"K4-retry-{attempt}", "POST", ADD, '{"ProductId":"p-4","Quantity":1}', key="K4")
        if s == 200:
            break
        time.sleep(2)
    check(s == 200, f"K4 retry after connection reset succeeds (got {s})")
    check(len(key_events(journal(), "K4")) == 1, "K4 produced exactly one journal event")

    before_restart = len(journal())
    print("== restart (cold caches, durable path)", flush=True)
    stop_server()
    start_server()
    # The restarted server re-verifies specs in the background; until then writes
    # get 423 VerificationRequired before dispatch (nothing is written).
    for _ in range(180):
        s, b = call("cold-K1-equivalent-retry", "POST", ADD_SHORT, EQUIVALENT, key="K1",
                    headers=TRANSPORT)
        if s != 423:
            break
        history.pop()
        time.sleep(1)
    check(s == 200 and logical(b) == logical(original),
          f"cold: equivalent K1 retry returns the ORIGINAL response after later actions (status {s})")
    s, b = call("cold-K1-different-body", "POST", ADD, OTHER_BODY, key="K1")
    check(s == 422 and error_code(b) == "IdempotencyKeyMismatch",
          f"cold: K1 with a different body -> 422 (got {s} {error_code(b)})")
    s, b = call("cold-K1-different-action", "POST", CANCEL, '{"Reason":"x"}', key="K1")
    check(s == 422 and error_code(b) == "IdempotencyKeyMismatch",
          f"cold: K1 with a different action -> 422 (got {s} {error_code(b)})")
    s, b = call("cold-intruder-K1", "POST", ADD, ORIGINAL, key="K1", token=INTRUDER_TOKEN)
    check(s == 403 and not (isinstance(b, dict) and "status" in b),
          f"cold: denied principal never receives a cached response (got {s})")
    s, b = call("cold-K3-retry", "POST", ADD, '{"ProductId":"p-3","Quantity":1}', key="K3")
    check(s == 200, f"cold: K3 retry succeeds (got {s})")
    check(len(journal()) == before_restart, "cold retries and mismatches appended no event")

    print("== journal / catalog / offline fold", flush=True)
    events = journal()
    s, entity = call("get-order", "GET", f"/tdata/Orders('{ORDER}')")
    s2, catalog = call("catalog", "GET", "/tdata/Orders")
    status_fold, items_fold, k1_fold = "", 0, None
    for e in events:
        p = e["payload"]
        status_fold = p.get("to_status") or status_fold
        if p.get("action") == "AddItem":
            items_fold += 1
        elif p.get("action") == "RemoveItem":
            items_fold -= 1
        if p.get("idempotency_key") == "K1" and k1_fold is None:
            k1_fold = (status_fold, items_fold)
    for key in ["K1", "K2", "K3", "K4"]:
        evs = key_events(events, key)
        check(len(evs) == 1, f"{key} appears in exactly one journal event")
        check(len(evs) == 1 and len(evs[0]["payload"].get("idempotency_binding", "")) == 64,
              f"{key} event carries a 64-hex request binding")
    check(not [e for e in events if e["payload"].get("action") == "CancelOrder"],
          "no CancelOrder event was journaled")
    seqs = [e["sequence_nr"] for e in events]
    check(seqs == list(range(1, len(seqs) + 1)), "journal sequence is contiguous from 1")
    check(entity.get("status") == status_fold, f"GET status == fold ({status_fold})")
    check(entity.get("counters", {}).get("items") == items_fold, f"GET items == fold ({items_fold})")
    row = next((r for r in (catalog or {}).get("value", [])
                if r.get("entity_id") == ORDER or r.get("Id") == ORDER), None)
    check(row is not None and row.get("status") == status_fold, "catalog row status == fold")
    check(k1_fold == (original.get("status"), original.get("counters", {}).get("items")),
          f"prefix fold through K1 {k1_fold} == original K1 response")

    # ---------------- PR #523 review corrections ----------------
    print("== review 3: racing writers on two server instances sharing PostgreSQL", flush=True)
    import socket
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port_b = sock.getsockname()[1]
    proc_b, base_b, log_b = start_instance("b", port_b, SPECS)
    s, _ = until_verified("B-hydrate-1", "GET", f"/tdata/Orders('{ORDER}')", base=base_b)
    check(s == 200, f"instance B hydrated the order (got {s})")
    s, w6 = call("A-K6-winner", "POST", ADD, '{"ProductId":"p-6","Quantity":1}', key="K6")
    check(s == 200, f"instance A commits K6 (got {s})")
    s, b = until_verified("B-K6-different-body", "POST", ADD, '{"ProductId":"p-6b","Quantity":1}',
                          key="K6", base=base_b)
    check(s == 422 and error_code(b) == "IdempotencyKeyMismatch",
          f"instance B racing with a different body -> 422 (got {s} {error_code(b)})")
    check(len(key_events(journal(), "K6")) == 1, "K6 has exactly one journal event across instances")
    s, _ = call("B-hydrate-2", "GET", f"/tdata/Orders('{ORDER}')", base=base_b)
    s, w7 = call("A-K7-winner", "POST", ADD, '{"ProductId":"p-7","Quantity":1}', key="K7")
    check(s == 200, f"instance A commits K7 (got {s})")
    s, b = call("B-K7-identical", "POST", ADD, '{"ProductId":"p-7","Quantity":1}', key="K7",
                base=base_b)
    check(s == 200 and logical(b) == logical(w7),
          f"instance B racing with the identical request returns the ORIGINAL response (status {s})")
    check(len(key_events(journal(), "K7")) == 1, "K7 has exactly one journal event across instances")
    stop_proc(proc_b)
    raced = open(log_b).read().count("optimistic-concurrency violation")
    check(raced >= 1, f"instance B entered the ADR-0046 concurrency retry ({raced} time(s))")

    print("== review 1: cold retry of a superseded key after cancellation", flush=True)
    s, b = call("K8-cancel", "POST", CANCEL, '{"Reason":"customer request"}', key="K8")
    check(s == 200 and b.get("status") == "Cancelled", f"K8 CancelOrder commits (got {s})")
    before_review1 = len(journal())
    stop_server()
    start_server()
    s, b = until_verified("cold-K1-after-cancel", "POST", ADD_SHORT, EQUIVALENT, key="K1",
                          headers=TRANSPORT)
    check(s == 200 and logical(b) == logical(original),
          f"cold K1 retry after cancellation returns the ORIGINAL Draft response (status {s})")
    s, live = call("get-after-superseded-retry", "GET", f"/tdata/Orders('{ORDER}')")
    check(live.get("status") == "Cancelled", f"live status stays Cancelled (got {live.get('status')})")
    s, cat = call("catalog-after-superseded-retry", "GET", "/tdata/Orders")
    row = next((r for r in (cat or {}).get("value", [])
                if r.get("entity_id") == ORDER or r.get("Id") == ORDER), None)
    check(row is not None and row.get("status") == "Cancelled", "catalog row stays Cancelled")
    check(len(journal()) == before_review1, "superseded-key retry appended no event")

    print("== review 2: supported spec update (AddItem items += 2) then cold retry", flush=True)
    stop_server()
    start_server(SPECS_DRIFT)
    before_review2 = len(journal())
    s, b = until_verified("drift-K2-retry", "POST", ADD, SECOND, key="K2")
    original_k2 = next(e["body"] for e in history if e["label"] == "K2-intervening")
    ok_original = s == 200 and logical(b) == logical(original_k2)
    ok_closed = s == 409 and error_code(b) == "IdempotencyKeyUnverifiable"
    check(ok_original or ok_closed,
          f"after the spec update K2 returns the original result or 409 Unverifiable, never a "
          f"re-derived 200 (got {s} {error_code(b)})")
    check(len(journal()) == before_review2, "spec-drift retry appended no event")
    s, live = call("get-under-drift-spec", "GET", f"/tdata/Orders('{ORDER}')")
    adds = len([e for e in journal() if e["payload"].get("action") == "AddItem"])
    check(live.get("counters", {}).get("items") == 2 * adds,
          f"drift spec is live: {adds} AddItem events replay to {2 * adds} items "
          f"(got {live.get('counters', {}).get('items')})")
    events = journal()
    stop_server()

    with open(f"{OUT}/history.json", "w") as f:
        json.dump(history, f, indent=1)
    with open(f"{OUT}/journal.json", "w") as f:
        json.dump(events, f, indent=1)
    with open(f"{OUT}/catalog.json", "w") as f:
        json.dump({"entity": entity, "catalog": catalog}, f, indent=1)
    summary = {"profile": PROFILE, "failures": failures, "journal_events": len(events),
               "fold": {"status": status_fold, "items": items_fold, "k1_prefix": k1_fold}}
    with open(f"{OUT}/summary.json", "w") as f:
        json.dump(summary, f, indent=1)
    print(json.dumps(summary), flush=True)
    sys.exit(1 if failures else 0)


try:
    main()
finally:
    if server is not None and server.poll() is None:
        server.kill()
```
