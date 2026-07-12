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

The action contract exposes only declared params to generated/verifier-facing
metadata, while the runtime accepted every body key. That mismatch let a caller
mutate fields outside the declared action surface. The current verifier does not
model arbitrary string-field assignments; ARN-212/ARN-213 track that larger
semantic gap. This ADR closes the runtime input-contract hole without claiming
those verifier limitations are solved. Concrete case (ARN-245): an
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
returns `Some(set)` for a known action. A rule-backed action with no entry is
invalid metadata and dispatch fails closed; an unknown action proceeds only to
the normal unknown-action rejection.

**Why**: IOA, codegen, and runtime now share one declared input contract instead
of the runtime trusting the raw body.

### Sub-Decision 2: DROP undeclared params at the single runtime chokepoint

`process_action_with_xref_and_field_mode` filters the body to the declared set
*before* guard evaluation, effect application, field projection, and event
recording. This one function is the shared path for every live dispatch (OData
bound actions, composite sub-writes, spawn initial actions, DST sim), so the model
is enforced everywhere. It is a **drop**, not a reject, because internal dispatch
also carries typed kernel linkage and parent-action context; the target action
must see only its declared subset. External callers get an explicit 400 instead.

Parent linkage remains typed kernel creation metadata. Explicit `copy_fields`
values flow through the child initial action and therefore must be declared by
that action. Bundle lint rejects a spawn contract that copies an undeclared
field; the runtime does not create a second direct-write path to preserve a bad
spec.

Ordering matters: the filter runs **ahead of** `normalize_ref_action_params`, so
kernel-derived params (the `Ref` `TargetCommitSha` synthesized from `NewCommitSha`)
are added after the filter and survive. Transient action fields (large/ephemeral
inputs consumed by triggers but never persisted, e.g. `Repository` pack bytes)
must be declared and are then preserved for trigger evaluation.

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

### Sub-Decision 4: Accept one explicit CSDL naming alias

IOA `params` are conventionally snake_case logical names, while their CSDL/OData
property spelling is the deterministic PascalCase form. The allow-set contains
only the exact declared name and `to_pascal_case(name)`. It does not case-fold or
strip punctuation, because those lossy transforms create collisions. A request
that supplies both spellings for one logical parameter is rejected as ambiguous,
and spec lint rejects declared parameters that map to the same canonical spelling
(for example `user_id` and `UserId`). Runtime metadata validation fails closed as
defense in depth if such a table is loaded without passing lint.

**Why not exact-only match**: it would reject existing CSDL/OData PascalCase
spellings for a snake_case IOA parameter. **Why not broad normalization**:
`user_id`, `userid`, and `USER_ID` are not interchangeable contract keys. **Why
not match the CSDL property set instead**: `goal` *is* a valid
`WorkSummary` property, just not a declared param of `AttachVector` — enforcement
must be per-action (declared params), not per-entity (schema), or the ARN-245 case
is not fixed.

## Consequences

- The declared action-input boundary is now enforced at runtime for every Temper
  app, on every dispatch path, without a per-spec opt-in. This narrows the
  verifier/runtime gap but does not add string-field semantics to the verifier.
- Well-formed callers are unaffected: audited Katagami curation, Aya, and Genesis
  callers already send exactly their declared params (modulo the one canonical
  snake_case-to-PascalCase CSDL alias).
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
- Spawn `copy_fields` must be declared by the target initial action. Bundle lint
  rejects an undeclared copy contract instead of bypassing action projection or
  silently dropping intended state.

## Round 2 — every dispatch/write path, not just the in-process action boundary

An adversarial review found the first cut only covered the in-process runtime.
The boundary now holds on every path an entity's fields can be written:

### pg-actor runtime (the load-bearing gap)

openpaw runs the **Postgres** actor runtime. Its bound-action path
(`odata/write.rs`, `is_pg_actor_backed`) previously ran Cedar authz then
`SpecMessage::with_params(action, body_json)` and returned *before*
`dispatch_bound_action`, so the declared-param boundary never ran; `spec_actor`
then merged every param into `fields` — even for an **unknown/invalid action** —
and always persisted. That is the exact ARN-247 primitive, live in prod.

Fix: the filter core moved to `temper_jit::params` so both runtimes share it.
`spec_actor` now (a) restricts incoming params to the declared set before they
touch fields (fail-closed on a contract violation) and (b) **persists only on a
successful transition** — a failed or unknown action is a no-op on state. The pg
bound-action path in `write.rs` also runs the external reject before the tell.

### Collection create (`POST /Set`)

Create wrote the request body verbatim into initial fields with no filter — the
same primitive at creation. It now rejects body keys the entity type does not
declare as a CSDL property/key (control keys `id`/`Id`/`status`/`Status` and
`@odata.*` excepted), failing open only when the type has no CSDL entity. `PATCH`
remains documented out of scope (tracked under the ARN-165 epic).

### Kernel-synthesized File params

`file_initial_writes.rs` synthesizes `version_number`/`previous_version_id`/
`created_by` for `StreamUpdated`. Their keys are kernel-fixed (the caller controls
only values), so they are re-applied **post-filter** into fields and the recorded
event — surviving even a tenant whose persisted File spec predates those
declarations. The bundled `file.ioa.toml` declares all six; the force-inject makes
it robust regardless of an older tenant's persisted `ioa_source`. Caller-controlled
`Create` params are *not* force-injected — they stay filtered.

### Spawn `copy_fields` vs parent linkage

Parent linkage (`parent_id`/`parent_type`/`{snake}_id`) is inserted into the
child's initial-action params **last**, after `copy_fields`, so a multi-level
spawn A→B→C whose `copy_fields` copies `parent_id` records B (not grandparent A)
as C's parent.

### Replay/snapshot re-poisoning (operational, not a replay filter)

`actor.rs` replay re-projects raw `event.params` with no filter. A naive replay
filter is **unsafe**: going-forward events legitimately carry kernel-injected
params that are *not* declared — Ref's `TargetCommitSha`, the File synthesizers —
so filtering at replay would strip them and corrupt those entities on every
rehydration. Instead: going-forward events are already clean (filtered at write
time), so their replay is clean; the residuals are operational —
  1. **Deploy** with drain-then-cutover so no un-fixed replica journals a poisoned
     event that a fixed replica faithfully replays during rollout.
  2. **Scrub** any already-poisoned entity's journal/snapshot (offline job); the
     runtime fix stops new poisoning but does not rewrite history.
These are tracked under the ARN-165 epic; they are deployment/data tasks, not a
kernel code change in this PR.
