# Issue 54: Grant-Scoped Module SDK Generation

Temper planning was unavailable because the configured local server could not
be reached. This file records the required fallback plan.

## Plan

1. Record the source-compatibility decision in ADR-0184.
2. Make each generated helper type and method conditional on the exact global
   operation and per-entity grant that permits it.
3. Require `metadata_read` for File entity metadata reads in both generation and
   host grant checks, preserving the host as the independent security boundary.
4. Split current-content and version-content generated reads so each method
   requires exactly its corresponding File operation.
5. Preserve entity/property types needed to decode granted reads, writes, and
   action results.
6. Add golden-style source assertions for get-only, action-only, query-scoped,
   and read-only File grants, including negative assertions for every
   ungranted helper family.
7. Prove deterministic source, manifest, grant digest, and packaged artifact
   generation; run formatting, focused tests, workspace checks, mandatory
   reviews, and the repository push gate.
8. Merge, deploy if this repository has a deployable target for codegen, and
   verify the released behavior with a generated least-privilege client.

## Acceptance Criteria

- Entity create and patch types/methods appear only with their operation grant.
- Query filter/order types and query method appear only with `entity_query`,
  and expose only declared filter/order fields.
- Only explicitly granted bound actions appear.
- File metadata reads require `metadata_read`; content and version reads require
  their exact File operation; every File API also requires its global operation.
- Entity result/property types required by granted calls remain available.
- Host-side authorization remains the security boundary and independently
  enforces every File operation represented by generated source.
- Generation and binding digests remain deterministic.
