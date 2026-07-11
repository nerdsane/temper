# ADR-0156: Declared-params-only at the runtime action boundary

- Status: Accepted
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0153: Declared composite key index (keys are computed from stored fields)
  - ADR-0155: Declared vector access path (the ARN-245 work that surfaced this)
  - `crates/temper-jit/src/table/{types,builder}.rs` (declared param set on the table)
  - `crates/temper-server/src/entity_actor/effects.rs` (the projection chokepoint)
  - `crates/temper-server/src/odata/bindings.rs` (the external boundary)
  - Linear ARN-247 (under the ARN-165 audit epic)

## Context

On a successful action the runtime projected **every** non-transient request-body
key verbatim into entity string fields (`sync_fields`), with no filter against the
action's declared `params`. Counters were re-synced from `state.counters`, so
counter state vars were safe, but every string state var was written straight from
the body.

Meanwhile the verification cascade models an action as writing only its declared
params. So L0–L3 proved invariants over a model the runtime did not enforce:
**an invariant could be provable yet violable.** Concrete case (ARN-245): an
`AttachVector(params=[semantic_vector, semantic_vector_model])` enabled from a
terminal-ish `Done` state could, at runtime, also be POSTed with
`{goal, outcome}` and overwrite those closed-record fields — the only guard is
Cedar (WHO may call), never WHICH params. Reproduced empirically before the fix.

## Decision

The runtime restricts an action's caller-supplied params to its declared set, so
the runtime matches the verified model. Two complementary points, one shared rule.

### Sub-Decision 1: Carry the declared param set on the TransitionTable

`from_automaton` records `action_params: BTreeMap<String, BTreeSet<String>>` (one
entry per action; empty set = declares no params). `declared_params(action)`
returns `Some(set)` for a known action, `None` when absent (older deserialized
table, or a synthetic kernel action) — in which case callers do **not** restrict,
preserving behavior.

**Why**: the verifier already reads `[[action]] params`; the runtime now sees the
same data instead of trusting the raw body.

### Sub-Decision 2: DROP undeclared params at the single runtime chokepoint

`process_action_with_xref_and_field_mode` filters the body to the declared set
*before* guard evaluation, effect application, field projection, and event
recording. This one function is the shared path for every live dispatch (OData
bound actions, composite sub-writes, spawn initial actions, DST sim), so the model
is enforced everywhere. It is a **drop**, not a reject, because internal dispatch
legitimately injects system params the child action never declared — spawn merges
`parent_id`/`parent_type` and `copy_fields` values into the child's initial-action
body, and rejecting there would break entity spawning.

Those spawn-injected values must not be *lost* by the drop either: parent linkage
and `copy_fields` values are persisted into the child at **creation**
(`initial_fields` in `dispatch/cross_entity.rs`), which is direct field writing and
never reaches this filter — so the child keeps them regardless of whether its
initial action declares them. The drop only ever narrows the action body.

Ordering matters: the filter runs **ahead of** `normalize_ref_action_params`, so
kernel-derived params (the `Ref` `TargetCommitSha` synthesized from `NewCommitSha`)
are added after the filter and survive. Transient action fields (large/ephemeral
inputs consumed by triggers but never persisted, e.g. `Repository` pack bytes) are
also preserved.

Replay is unaffected: it bypasses this function and reconstructs faithfully from
stored events; new events are recorded from the already-filtered params, so their
replay is clean without rewriting history.

### Sub-Decision 3: REJECT undeclared params at the external OData boundary

`dispatch_bound_action` returns `400 UndeclaredActionParams` (naming the offending
keys) when a bound-action request body carries keys outside the allowed set. This
is the loud, surface-every-error half: a typo'd or smuggled param on the external
surface is an error, not a silent no-op. Internal dispatch paths do not pass
through this handler, so they keep the lenient drop. The reject and the drop share
one allow-rule so they never diverge. Entity creation (`POST /Set`) and PATCH are
direct field writes, not action dispatch, and are out of scope.

### Sub-Decision 4: Match declared params up to naming convention

IOA `params` are snake_case *logical* names, but some callers dispatch with the
PascalCase field names they store — paw-fs dispatches `Directory.Create` with
`WorkspaceId`/`ParentId` for declared `workspace_id`/`parent_id` (the keys ADR-0153
then hashes). Matching is therefore done on a normalized key (lowercase, drop
underscores), so those callers keep working while a genuinely undeclared key
(`goal` on an action that never declared it) still matches nothing.

**Why not exact match**: it would drop paw-fs's PascalCase fields, emptying the
directory fields the declared key is computed from — breaking keyed existence in
production. **Why not match the CSDL property set instead**: `goal` *is* a valid
`WorkSummary` property, just not a declared param of `AttachVector` — enforcement
must be per-action (declared params), not per-entity (schema), or the ARN-245 case
is not fixed.

## Consequences

- A proven invariant over declared params is now enforced at runtime for every
  Temper app, on every dispatch path, without a per-spec opt-in.
- Well-formed callers are unaffected: audited Katagami curation, Aya, and Genesis
  callers already send exactly their declared params (modulo the snake/Pascal
  convention that normalization bridges).
- Undeclared params on a bound action are now a 400 rather than a silent write,
  and undeclared params on internal dispatch are silently dropped. **Actions must
  declare the params for the fields they set.** Audited production apps already
  do: the Aya specs' `WikiPage.Draft`/`Revise` and `Note.Save` param lists match
  `aya.py`'s bodies field-for-field, and the Katagami curation WASM sends exactly
  each action's declared params. The one case found relying on verbatim
  projection was a hand-written kernel *test* spec (`trigger_e2e_prod`'s
  `Order.AddItem`, which set `payment_id` without declaring it) — fixed by
  declaring the param. A full audit of every deployed spec (all os-apps, not just
  the sampled apps) remains a cheap, recommended **pre-deploy** gate.
- Kernel entity types with hardcoded domain logic that reads specific params were
  verified to declare them: the deployed Genesis `Ref` spec declares
  `PreviousCommitSha`/`NewCommitSha` on `Update`/`ForceUpdate`, and `Repository`
  declares its `IngestPack`/`WriteFile` params (incl. the transient `PackBytes`).
  If those specs ever narrowed their declared params, filtering would strip the
  input the kernel logic needs — a cross-repo invariant worth a boot-time check.
- The DROP enforces the invariant on *action dispatch* only. Direct `POST /Set`
  creation and `PATCH` write fields without an action, so the ARN-245-class
  invariant remains bypassable via PATCH — pre-existing, tracked under the ARN-165
  audit epic, not closed here.
- Minor: an *undeclared* `copy_fields` value now persists inline at child creation
  rather than through overflow-aware `sync_fields`. Declared copied fields still go
  through the overflow path; copy_fields are config-shaped in practice, so this is
  a storage-representation nuance, not a correctness issue.
