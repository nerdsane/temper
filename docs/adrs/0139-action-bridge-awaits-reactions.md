# ADR-0139: The HTTP Action Bridge Awaits Reactions

## Status

Accepted

## Context

A Composite action's declared sub-writes are produced by its integration and
applied by the kernel when that integration runs through an *awaited* path.
There are two awaited paths:

- The OData bound-action path (`dispatch_bound_action`) sets
  `await_reactions: true`, so an action's `[[action.triggers]]` reaction runs
  synchronously and the kernel applies the returned `sub_writes` before the
  HTTP response is produced. This is how `App.RegisterNewApp` / `App.Install`
  create their rows.
- The HTTP action bridge (`dispatch_action_bridge_result`) set
  `await_reactions: false`. A Composite action dispatched through the bridge
  therefore had its reaction spawned in the background, and the returned
  `sub_writes` were silently dropped.

Every git protocol surface routes through the bridge:
`Repository.IngestPack` (git push) and `Repository.MergePullRequest`
(gh merge) are Composite actions whose sub-writes carry the result. With
`await_reactions: false`, a push parsed and staged its objects but
**created no Blob/Tree/Commit/Ref rows** — a cold-boot clone of a freshly
pushed repository returned nothing. This was latent because the path had
only ever been exercised against warm, pre-seeded servers; the first
cold-boot end-to-end run surfaced it.

## Decision

The HTTP action bridge sets `await_reactions: true`, matching the OData
bound-action path. A Composite action dispatched through the bridge now has
its `[[action.triggers]]` reaction run synchronously, its `sub_writes`
applied, and the protocol response produced only after the writes are durable.

For git push specifically this is also the correct protocol semantics: the
`git-receive-pack` response must not report success before the refs and
objects are persisted.

## Consequences

- `Repository.IngestPack` and `Repository.MergePullRequest` apply their
  composite sub-writes end-to-end when invoked through the wire/REST surface,
  not just through OData.
- Bridge-dispatched Composite actions now block on their reaction. For git
  pushes and merges this is desired (the client should wait for durability).
  Adapters whose reactions are genuinely fire-and-forget should not be modeled
  as Composite actions on the bridge.
- DST-neutral: the change flips one dispatch option to match the existing
  awaited path; no new clock, randomness, or I/O, and the reaction machinery
  is unchanged.
