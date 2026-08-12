# ADR-0061: Agent Context Consumers Use the TemperFS Batch Read Plane

- Status: Accepted
- Date: 2026-04-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0029: TemperFS - A Governed File System on Temper Primitives
  - ADR-0170: Native Immutable File Read Plane for TemperFS
  - `crates/temper-server/src/api/files.rs`
  - `crates/temper-server/src/state/file_reads.rs`

## Context

ADR-0170 added the native TemperFS read plane for content-heavy consumers:

- `POST /api/files/read-text-batch` for current `File` head reads
- `POST /api/files/read-version-text-batch` for immutable `FileVersion` reads

OpenPaw agent context preparation is the first latency-sensitive consumer of
this read plane. A long session tree can reference many externalized content
files. Reading those files one by one through generic entity or `$value` paths
turns context preparation into a per-file control-plane loop and can exceed the
WASM and state timeout budgets before a provider call starts.

## Decision

Agent context consumers must use the TemperFS batch read plane for session
content hydration.

The intended contract is:

1. Prefer immutable `FileVersion` IDs when a session entry records them.
2. Use current `File` batch reads only when immutable version IDs are absent.
3. Preserve Temper ownership: writes, lineage, lifecycle, and authorization stay
   entity-first; the native read plane is read-only over committed state and
   blob content.
4. Treat per-file serial `$value` reads as a bounded compatibility fallback, not
   the normal path for context assembly.

## Consequences

- Platform work from ADR-0170 remains the single clean read primitive for agent
  context prep and future filesystem-shaped consumers.
- OpenPaw regressions can be detected by checking whether the active context
  preparation module uses batch current-file/version reads rather than a serial
  `$value` loop.
- Timeout tuning is not an acceptable substitute for using the read plane.

## Non-Goals

- Adding OpenPaw-specific orchestration to Temper.
- Replacing OData or `$value` for ordinary one-off file access.
- Introducing an external cache outside TemperFS ownership.
