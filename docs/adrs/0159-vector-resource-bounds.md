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

### Sub-Decision 2: Per-query work is bounded by two explicit caps

With `dims` bounded at verification and the row count already bounded by ADR-0155's
`candidate_budget`, the query's bytes and dot-product operations are bounded by
construction: `work ≤ candidate_budget × MAX_VECTOR_DIMS`, both explicit
constants. This also bounds the blobs materialized before the row-count check —
each candidate blob is at most `MAX_VECTOR_DIMS × 4` bytes, because the write path
only ever indexes an exactly-`dims`-length vector (`parse_vector_property(v, dims)`
rejects any other length in `crates/temper-server/src/vector_index.rs`) — so the
"stores materialize each full blob before the count is checked" concern is closed
without changing the query path. A separate per-query element budget was considered but
rejected (below) as redundant and risk-prone to size.

## Consequences

### Positive
- A spec can no longer declare an unbounded-dimensionality vector; query work is
  bounded by construction — `rows ≤ candidate_budget` (ADR-0155) and
  `dims ≤ MAX_VECTOR_DIMS` — so `rows × dims`, and each pre-check blob
  (`≤ MAX_VECTOR_DIMS × 4` bytes), are bounded.
- Enforced once, at verification. `dims` is a spec property, so every storage
  backend inherits the bound with no per-backend or query-path change.

### Negative
- A (hypothetical) future embedding model above `MAX_VECTOR_DIMS` would need the
  constant raised. The cap is chosen with generous headroom to make that remote.

### DST Compliance
- The only change is in `temper-spec` (not simulation-visible): integer
  comparisons and length checks in `validate_vector_decls`, no wall clock,
  threads, `HashMap`, or ambient I/O. No `temper-server`/`temper-runtime` code is
  touched, so there is nothing new on the simulation-visible query path.

## Non-Goals

- The existing row/candidate budget (ADR-0155) and the existing non-finite-value
  rejection in `unpack_f32_le` are unchanged; this ADR adds the missing
  dimensionality and identifier bounds on top of them.
- Bounding the *runtime* model-tag length (the `model` query argument / the
  reference entity's `model_property` value, used as a partition filter in
  `nearest.rs`) is a separate concern: its cost is O(len) per candidate string
  comparison, bounded by `candidate_budget`, not the `rows × dims` allocation
  ARN-217 targets. Bounding runtime state-variable content is a broader question
  and is left as a follow-up.

## Alternatives Considered

1. **A separate per-query `candidate_count × dims` element budget on the query
   path.** Rejected: the product is already bounded by the two caps above, so the
   check is redundant; and sizing its constant without wrongly rejecting a
   legitimate kNN query depends on the deployment's `candidate_budget`, so a fixed
   value risks either being ineffective or breaking real queries. The two
   explicit caps are the honest, safe bound.
