# ADR-0159: Vector declaration and query resource bounds

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0155: Declared vector access path (added the row/candidate budget)
  - `crates/temper-spec/src/automaton/parser.rs` (`validate_vector_decls`)
  - `crates/temper-server/src/odata/nearest.rs` (kNN query path)
  - `crates/temper-runtime/src/persistence/mod.rs` (`unpack_f32_le`)
  - ARN-217 (security finding)

## Context

ADR-0155 caps the *row* count a `Temper.Nearest` scan will materialize
(`candidate_budget`), and the shared `unpack_f32_le` already rejects non-finite
vector values. But `VectorDecl.dims` has **no upper bound** — a spec may declare
a vector path with, e.g., `dims = 100_000_000`, which the verification cascade
accepts today (ARN-217). Query work and allocation are `rows × dims`: with the
row count bounded but the dimensionality unbounded, a single spec can still force
multi-gigabyte allocation and dot-product work per query (400 MB per 100 M-dim
vector blob). Identifier lengths on the declaration are likewise unbounded.

## Decision

### Sub-Decision 1: Bound the declaration at verification (fail-closed)

`validate_vector_decls` rejects any `[[vector]]` whose `dims` exceeds
`MAX_VECTOR_DIMS`, and whose `name` / `property` / `model_property` identifiers
exceed `MAX_VECTOR_IDENT_LEN`. `dims` is a spec property enforced once, so every
storage backend inherits the bound with no per-backend code. `MAX_VECTOR_DIMS` is
set well above any real embedding model (which top out in the low thousands) so
no legitimate spec is rejected, while bounding a single vector blob to a small,
fixed size.

### Sub-Decision 2: Explicit aggregate query budget (defense in depth)

The kNN query path additionally rejects a request whose total work
`candidate_count × dims` would exceed `MAX_QUERY_VECTOR_ELEMENTS`, with a
structured `PAYLOAD_TOO_LARGE` error rather than silently proceeding. This bounds
per-query bytes/operations directly (the finding's "budgets consumed by
operations/bytes"), independent of how the row and dim caps are configured.

## Consequences

### Positive
- A spec can no longer declare an unbounded-dimensionality vector; query work is
  bounded by construction (`rows ≤ candidate_budget`, `dims ≤ MAX_VECTOR_DIMS`),
  and the explicit aggregate budget backstops the product.
- Enforced once at verification + once on the shared query path — every backend
  is covered without per-backend changes.

### Negative
- A (hypothetical) future embedding model above `MAX_VECTOR_DIMS` would need the
  constant raised. The cap is chosen with generous headroom to make that remote.

### DST Compliance
- The verification change is in `temper-spec` (not simulation-visible). The query
  budget in `temper-server` compares integers and returns a structured error —
  no wall clock, threads, `HashMap`, or ambient I/O.

## Non-Goals

- The existing row/candidate budget (ADR-0155) and the existing non-finite-value
  rejection in `unpack_f32_le` are unchanged; this ADR adds the missing
  dimensionality and aggregate bounds on top of them.
