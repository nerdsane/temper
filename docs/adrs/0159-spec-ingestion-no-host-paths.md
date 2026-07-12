# ADR-0159: Spec ingestion without host paths (ARN-229)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ARN-229: Spec ingestion exposes arbitrary host filesystem
  - `crates/temper-server/src/observe/specs/load_dir.rs`
  - `crates/temper-server/src/observe/specs/load_inline.rs`
  - `crates/temper-server/src/observe/specs/path_security.rs`

## Context

`POST /api/specs/load-dir` accepted an arbitrary server path and read CSDL/IOA
from it without a handler-level auth gate. `load-inline` staged into a
tenant-named shared temp directory and joined caller keys without rejecting
`..` / absolute paths, enabling path escape and concurrent clobbering.

## Decision

### Sub-Decision 1: No network-named host directories

The HTTP `load-dir` endpoint **rejects** caller-supplied server paths
(status `410 Gone` / clear error). Specs enter the network API only as an
in-memory filename→content map (`load-inline`).

Internal loading from a resolved path remains available only after the kernel
itself materializes a validated staging directory (used by load-inline).

### Sub-Decision 2: Strict relative keys for inline bundles

Every map key must be a relative path:

- No absolute paths, drive prefixes, or `..` components
- No null bytes or empty segments
- Normalized containment under the staging root after join
- Budgets: max files, max total bytes, max single file size

### Sub-Decision 3: Invocation-unique staging

Staging directories are `temper-inline-{tenant}-{uuid}` (not tenant-only), so
concurrent submissions cannot delete each other's trees.

### Sub-Decision 3b: No durable registry for ephemeral staging

`specs-registry.json` must not record `temper-inline-*` paths. Staging is
deleted when the request finishes; writing those paths would poison restart
reload. Existing poisoned entries are scrubbed on the next registry rewrite.

### Sub-Decision 4: Auth on load-inline remains required

Cedar `submit_specs` continues to gate load-inline. load-dir no longer accepts
network callers.

## Consequences

### Positive

- Network callers cannot name host directories.
- Path traversal and shared-temp races are closed for inline submission.

### Negative

- Local operators who scripted `load-dir` with filesystem paths must switch to
  load-inline or an out-of-band install path.

## Non-Goals

- Full multipart archive format (zip) — in-memory map is sufficient for now.
