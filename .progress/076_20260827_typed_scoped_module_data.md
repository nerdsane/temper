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
