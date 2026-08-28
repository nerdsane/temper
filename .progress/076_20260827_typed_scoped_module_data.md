# Issue 76: Host-Owned Typed Scoped Module Data

Temper planning was unavailable because no Temper MCP server is connected to
this session. This file records the required fallback plan for GitHub issue 76.

## Plan

1. Record the host-owned scoped binding and immutable recovery contract in
   ADR-0191.
2. Extend scoped bundle module descriptors with a canonical typed-data binding
   whose digest participates in bundle identity.
3. Verify the artifact-carried binding against the exact canonical scoped
   schema closure and capability grant before activation.
4. Resolve scoped module artifacts and manifests from the invocation's
   host-owned `SchemaExecutionPin`; never accept scope, tenant, or principal
   values from the guest ABI.
5. Route keyed reads, bounded queries, creates, patches, actions, composites,
   and native File operations through the exact immutable pin while preserving
   tenant-global behavior when no pin exists.
6. Preserve typed failure details, decisions, authoritative results and commit
   sequences, and all call/item/byte/handle/stream budgets.
7. Add generated-client, spoofing, cross-scope, drift, budget, and persistent
   restart coverage.
8. Run focused and workspace validation, mandatory DST and code reviews, the
   push gate, live local E2E, deployment, and Datadog verification.

## Acceptance Criteria

- A generated client for a dynamically deployed scoped schema operates only on
  the host invocation's immutable scope and needs no `/tdata` request.
- Guest input cannot redirect tenant, scope, principal, schema, or grant.
- Cross-scope and cross-tenant reads and writes fail closed.
- Artifact, schema, closure, used-symbol, or grant drift prevents verification,
  activation, binding, or invocation.
- Keyed reads, bounded queries, creates, patches, actions, composites, native
  File operations, authoritative results, and commit sequences retain typed
  module-data semantics.
- Budget failures and Cedar denials preserve structured `ModuleDataError`
  fields through the guest ABI.
- Persistent restart recovers the exact immutable scoped module binding and
  resumes typed access to the same scope.
- Existing tenant-global typed module-data behavior is unchanged.

## Implementation Status

- ADR-0191 and the fallback issue plan are complete.
- Scoped bundle identity now binds the exact typed-data manifest digest and
  durable bundle records retain the canonical manifest.
- Verification regenerates the generated-client contract from the exact
  canonical scoped closure and validates the content-addressed artifact
  binding before activation.
- WASM dispatch derives a host-owned global/scoped target from actor state and
  request context, resolves exact scoped artifacts, and rejects missing or
  mismatched pins before guest execution.
- Entity, query, action, composite, batch, and native File paths route through
  the immutable target without tenant-global fallback.
- Deterministic simulated-store coverage proves cross-scope isolation, global
  non-leakage, typed missing-bundle failure, and recovery after scoped actor
  eviction.
- A checked-in generated WASM client now exercises submission, host
  verification, activation, create/get guest ABI calls, cold registry/store/
  artifact-cache recovery, and a second post-restart guest invocation.
- Scoped tests cover native File operations, composite calls, structured Cedar
  denial details, call and query budgets, cross-tenant isolation, and seeded
  restart/concurrency schedules.
- Enlarged production responsibilities are split into focused modules; every
  newly touched production file is below 500 lines.

## Mandatory Review Closure

1. Build one canonical scoped deployment fixture that packages generated SDK
   code into a real WASM guest, submits the bundle, performs host verification,
   activates it, and invokes the guest ABI without a `/tdata` request.
2. Rebuild a cold `ServerState`, scoped registry, and WASM artifact cache from
   the durable deployment store, then repeat the guest invocation against the
   exact recovered pin.
3. Reuse the same fixture for scoped File, composite, Cedar-detail, budget, and
   cross-tenant fail-closed coverage.
4. Add seeded randomized restart and injected-store-fault schedules that assert
   scope, tenant, digest, and commit isolation after every recovery boundary.
5. Split newly enlarged production responsibilities into directory modules so
   every touched production file satisfies the 500-line guideline.

## Validation

- `cargo test -p temper-spec --test scoped_spec_bundle` — 14 passed.
- Focused registry and dispatch provenance tests — passed.
- `cargo test -p temper-server --features sim,observe --lib application_data::`
  — 41 passed.
- Scoped application-data suite — 8 passed, including File, composite, Cedar,
  budget, cross-tenant, and seeded recovery coverage.
- Generated-client cold-restart E2E — passed, including 20 consecutive stress
  runs and an independent DST-review run.
- Exact scoped WASM dispatch integration — passed.
- `cargo check -p temper-server --features observe,sim` — passed.
- `cargo fmt --all -- --check`, integrity, and readability ratchet — passed.
- Workspace/all-target Clippy with `-D warnings` — passed; vendored
  `libsql` emitted pre-existing dependency warnings.
- `CARGO_INCREMENTAL=0 python3 scripts/validation.py run prepush-workspace` —
  passed in 581.7 seconds, within the 1,800-second budget.
- Mandatory code-quality review — PASS with no findings.
- Mandatory DST review — issue-76 blocker closed and code-path unity CLEAN;
  the repository-wide DST maturity verdict remains `DST-INCOMPLETE` for
  pre-existing broader coverage targets outside this issue.
