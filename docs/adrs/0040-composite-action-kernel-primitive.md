# ADR-0040: Composite-action kernel primitive

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Supersedes: (none — extends ADR-0019 and related WASM-integration ADRs)
- Related:
  - ADR-0002: WASM integration for agent-generated API calls
  - ADR-0033: Tenant database isolation
  - ADR-0039: Latency observability acceleration program
  - `nerdsane/temper-git` RFC-0003: Genesis app registry
  - `nerdsane/temper-git` RFC-0002: push and clone (the missing
    `Repository.IngestPack` action that needs this primitive)
  - `crates/temper-spec`, `crates/temper-runtime`,
    `crates/temper-jit`, `crates/temper-server`

## Context

Temper's current action model is one-action-one-write: each invocation
runs through HTTP router → OData parser → Cedar evaluation → state
machine transition → event log append → projection update. This is
correct, but it has a structural inefficiency for compound intents.

Several apps have hand-rolled composite actions that work today:

- `Repository.WriteFile(Ref, Path, Content, Mode, Message, Author, ...)`
  in temper-git — atomic single-file commit. Internally builds a tree,
  writes a commit, advances a ref. **One Cedar gate; multiple writes;
  one transaction; one event.**
- `Repository.BatchWriteFiles(Ref, Changes, Message, Author, ...)` —
  atomic multi-file commit.
- `Repository.MergePullRequest(PullRequestId, Strategy, ...)` —
  applies source onto target, emits merged commit, advances target ref.
- `Blobs.IngestRaw(stream)` — streaming binary action that avoids
  JSON+base64 blowup.

Each of these was hand-rolled in app code. The pattern is right —
one Cedar gate, atomic multi-write, one event — but it isn't a
first-class kernel feature. Every app that needs it reinvents the
plumbing.

### Why this matters now

`nerdsane/temper-git` RFC-0003 (Genesis app registry) needs at least
two new composite actions:

- `Repository.IngestPack(pack_bytes, ref_updates[])` — the missing
  piece in temper-git RFC-0002. Today's `git_receive_pack.wasm` does
  naive N-way fanout (one OData write per pack object); this is the
  documented single biggest latency win available to temper-git.
- `Apps.Fork(parent_app, parent_version)` — creates `Repository` +
  `Ref` + `App` + `Lineage` in one atomic step (see temper-git
  ADR-0006).

Both want the same property: one Cedar gate at the level the agent
actually expresses intent, then multiple sub-writes in one
transaction, one event in the log.

Without a first-class kernel primitive, both will be hand-rolled the
same way the existing ones were, and the pattern stays implicit
instead of explicit.

### The transmission log finding

The transmission log analysis (`temper-git-transmission-log.html`, Q4
and Q7) frames this as "**push as the unit of governance, objects as
content-addressed bytes**." Cedar belongs on intents; content-addressed
bytes don't need per-byte governance. When the unit of governance
matches the unit of intent, the pipeline cost amortizes. When it
mismatches — when one user-intent translates to dozens of governed
operations — the pipeline cost multiplies.

The composite-action primitive is the kernel-level mechanism that
admits the right unit of governance for each operation. It is the
single most load-bearing primitive for the "exceptional latency"
property described in transmission log Q5.

## Decision

**Add `Composite` as a first-class action kind in the spec runtime
and kernel, generalizing the hand-rolled pattern.**

### Sub-decision 1: Spec syntax

A composite action declares its sub-writes in its spec:

```toml
# specs/repository.ioa.toml
[[action]]
name = "IngestPack"
kind = "Composite"
entity = "Repository"
inputs = [
  { name = "pack_bytes", type = "Binary" },
  { name = "ref_updates", type = "Array<RefUpdate>" }
]

[[action.cedar_gate]]
# One Cedar evaluation at this level
principal = "request.principal"
resource = "this"  # the Repository
action = "Repository::IngestPack"

[[action.sub_writes]]
target_entity = "Blob"
action = "Create"
generated_from = "pack_bytes"  # handler decomposes pack → objects

[[action.sub_writes]]
target_entity = "Tree"
action = "Create"
generated_from = "pack_bytes"

[[action.sub_writes]]
target_entity = "Commit"
action = "Create"
generated_from = "pack_bytes"

[[action.sub_writes]]
target_entity = "Ref"
action = "Update"
generated_from = "ref_updates"
```

The spec declares (a) the gate that runs at composite entry and (b)
the *kinds* of sub-writes the action will produce. The handler
(WASM or Rust integration module) provides the actual sub-write
content.

### Sub-decision 2: Runtime semantics

When a composite action runs:

1. **Single Cedar evaluation** at composite entry. The principal,
   resource, and composite action_id are evaluated. If denied, abort
   before any sub-writes.
2. **Single transaction.** All sub-writes (object inserts, ref
   updates, downstream entity writes) execute in one atomic batch.
3. **No per-sub-write Cedar.** Sub-writes inside a composite skip the
   per-action Cedar loop. The composite-level gate has already
   authorized them.
4. **Spec validation per sub-write.** Field types, formulas, refs are
   still validated per row; only the *authorization* layer is hoisted.
5. **Single event emitted.** The log entry is a `CompositeEvent` with
   the composite action_id, principal, and a structured summary of
   sub-writes. Replay loads one event, applies all sub-writes.
6. **Projections updated in parallel where possible**, serialized
   per-table.

### Sub-decision 3: Handler interface

A composite handler in Rust integration code or WASM exposes:

```rust
trait CompositeHandler {
    fn execute(
        &self,
        ctx: &CompositeContext,  // gives access to typed args
        writes: &mut SubWriteBuilder, // typed buffer for sub-writes
    ) -> Result<CompositeOutcome>;
}
```

`SubWriteBuilder` accumulates typed sub-writes. When `execute`
returns, the kernel flushes them in one atomic batch.

### Sub-decision 4: Auditability and provenance

Every sub-write inside a composite carries a back-reference to the
composite event ID. Queries like "who created blob X?" resolve to
"the principal of `CompositeEvent` Y, which contained X."

This is a strict improvement on the current model: the log unit now
matches the human/agent unit of intent, so queries that ask "what
was actually intended here" get a meaningful answer.

### Sub-decision 5: Existing hand-rolled composites adopt the primitive

`WriteFile`, `BatchWriteFiles`, `MergePullRequest` in temper-git
remain semantically identical, but are re-implemented to use the new
`Composite` action kind. This proves the primitive is general and
removes the redundant plumbing.

## Rollout Plan

1. **Phase 0 (this PR).** Spec syntax + runtime semantics documented.
   No code yet. Sets the contract.
2. **Phase 1.** Kernel implementation:
   - `Composite` action kind in `temper-spec`
   - Transaction batching in `temper-runtime` and `temper-server`
   - Single-Cedar gate enforcement
   - `CompositeEvent` log format
   - Replay support
3. **Phase 2.** Reference handlers:
   - Adopt the primitive in `Repository.WriteFile`,
     `BatchWriteFiles`, `MergePullRequest` (regression coverage that
     semantics are unchanged)
4. **Phase 3.** New composite actions in temper-git:
   - `Repository.IngestPack` (closes temper-git RFC-0002)
   - `Apps.Fork` (per temper-git ADR-0006)
   - `Apps.PublishNewVersion` (per RFC-0003 §9)
5. **Phase 4.** Latency observability harness verifies expected
   improvement on `git push` (the test in temper-git RFC-0002 §slices).

## Readiness Gates

- Existing apps' hand-rolled composites pass regression tests after
  re-implementation against the primitive.
- `Repository.IngestPack` end-to-end test demonstrates ≤ 10ms for a
  50-object push on a same-machine kernel (regime A).
- DST-mode tests verify determinism: composite execution produces
  identical event log and projection state across simulation seeds.
- The audit trail correctly attributes every sub-write to the
  composite event.

## Consequences

### Positive

- **Latency.** One Cedar evaluation + one transaction + one fsync
  per composite instead of N. Matches the transmission log Q5
  speed-budget analysis (~110µs floor for a fully-governed git push
  on the agent-resident path).
- **Auditability.** Log unit matches intent unit. Replay is one event
  instead of N ordered ones.
- **Generalizes beyond git.** Any compound intent — multi-row
  publish, multi-step state transition, ingest-and-attach — uses the
  same primitive. File uploads, LLM message logs, telemetry events,
  cached web fetches all benefit from the same shape (transmission
  log Q4).
- **Removes redundant plumbing.** Existing hand-rolled composites
  become idiomatic instead of bespoke.

### Negative

- **Spec authors must think about gate placement.** Adding more
  composite actions means more "what's the right gate level" design
  decisions. Mitigated by documentation and by the fact that the
  pattern is already familiar from existing apps.
- **Migration cost.** Re-implementing existing composites to use the
  primitive requires careful regression coverage.

### Risks

- **Cedar gate at the wrong level.** If a composite's gate is too
  coarse, the policy says "yes" to operations that should have been
  denied per-sub-write. Mitigated by spec design discipline: sub-
  writes inside a composite are constrained by the composite's
  declared sub-write kinds; arbitrary writes are not permitted.
- **Transaction size.** A composite with thousands of sub-writes
  could blow transaction budgets. Mitigated by per-app bounded
  mailbox limits (TigerStyle); composites that exceed limits chunk
  internally or are split into multiple composites.

### DST Compliance

- The composite executor runs deterministically: all sub-write
  ordering is governed by spec-declared order; no parallelism crosses
  determinism boundaries inside a composite.
- `CompositeEvent.id` uses `sim_uuid()` in simulation-visible code
  paths.
- Replay produces identical state from `CompositeEvent`s as from
  the original sub-write sequence.
- No `// determinism-ok` annotations expected; this is pure runtime
  logic.

## Non-Goals

- Pre-compiled Cedar (separate µs-floor primitive; future ADR).
- Hot in-memory projections (separate; future ADR).
- Group-commit on the event log (separate; future ADR).
- WASM-initiated action orchestration. Specs declare actions,
  triggers, integrations, and composite sub-writes; WASM integrations
  return data for the kernel to apply through those spec contracts.

## Alternatives Considered

### Keep composites hand-rolled per app (rejected)

Continue letting each app's integration code do its own multi-write
plumbing.

**Rejected because:** every new compound intent reinvents the plumbing;
no shared correctness guarantees; the pattern stays implicit and the
kernel can't optimize what it doesn't know about.

### Cedar permits batch-write directly (rejected)

Express batched writes as Cedar policy patterns rather than a kernel
primitive.

**Rejected because:** Cedar's job is authorization; not transaction
management. Pushing batch semantics into policy conflates two
concerns and hurts policy readability.

### "All writes are batched at the kernel level by default" (rejected)

Treat every action invocation as implicitly a batch.

**Rejected because:** the Cedar gate placement is the actual
load-bearing semantic, not just the batching. Explicit composite
declaration makes the gate placement legible; implicit batching
hides it.

## Rollback Policy

If the composite primitive proves wrong:

1. Revert spec syntax and runtime support.
2. Existing hand-rolled composites continue to work (they're plain
   actions internally).
3. Re-evaluate whether the right level was action-batching or
   something else (transaction sets? saga patterns?).

The primitive is additive and doesn't replace existing capability.
Rollback is therefore low-risk.
