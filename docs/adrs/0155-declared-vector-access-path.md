# ADR-0155: Declared vector access path (kernel-native kNN)

- Status: Accepted
- Date: 2026-07-06
- Deciders: Temper core maintainers
- Related:
  - ADR-0153: Declared composite-key index — the pattern this extends (a spec
    declares an access path; the kernel maintains it from persisted state;
    every read is budgeted, Cedar-governed, deterministic).
  - ADR-0154: OData read-surface truthfulness — a new read surface must not lie.
  - RFC-0003: Vector access paths in the Temper query plane (signed off).
  - `crates/temper-spec/src/automaton/types.rs` (`[[vector]]` decl)
  - `crates/temper-server/src/vector_index.rs` (pack/parse/rank)
  - `crates/temper-server/src/odata/nearest.rs` (`Temper.Nearest`)
  - `crates/temper-store-{sim,postgres,turso}` (index maintenance)

## Context

Entity catalogs are searchable by equality `$filter` and substring facets only.
Producers (Katagami taste vectors, Aya retrieval) now write embedding vectors onto
entities as ordinary action-event state, but similarity ranking runs out of process
(UI-side cosine, an interim MCP tool) — ungoverned, unbudgeted, and duplicated per
consumer. ADR-0153 already showed how to make an access path first-class: a spec
declares it, the kernel derives an index from persisted state, and reads of it are
bounded and deterministic. This ADR applies that same shape to vector similarity.

Embedding *generation* stays outside the kernel forever (it is a nondeterministic
API call; a live call in the write path would break seed-reproducible verification).
The kernel only indexes vectors that already live on an entity as opaque `f32[dims]`.

Scale honesty: tenants hold ≤1k entities; exact cosine over 1k×384-d is microseconds.
This buys governance, verifiability, and a platform primitive every tenant gets for
free — not performance. ANN is a later, declared opt-in, out of scope here.

## Decision

### Sub-Decision 1: `[[vector]]` declaration, verified by the cascade

A sibling of `[[key]]` on the automaton:

```toml
[[vector]]
name = "taste"
property = "taste_vector"              # JSON array (or JSON-string) of floats on the entity
model_property = "taste_vector_model"  # partitions the space; only same-tag vectors compare
dims = 384
metric = "cosine"                      # cosine | dot | l2
```

The verification cascade checks: `property` and `model_property` are declared state
variables, `dims > 0`, and `metric ∈ {cosine, dot, l2}`. Declaring a vector path on a
property that actions never write is legal — it indexes nothing (same posture as keys).

**Why**: named parameters on a declaration map 1:1 onto the index partition key and the
read surface; the cascade is the one place spec mistakes are caught before deploy.

### Sub-Decision 2: derived, model-partitioned index maintained from persisted state

Index rows live in `entity_vector_index (tenant, entity_type, decl_name, model_tag,
entity_id, vector BLOB)`, PK on everything but `vector`; the blob is packed
little-endian f32. The journal keeps the human-readable JSON on the action event; the
index is derived, rebuildable state. Model tags partition the space — a kNN query
resolves against exactly one `model_tag` (named, or defaulted to the reference
entity's tag). Vectors from different models are never compared.

Maintenance mirrors ADR-0153: co-committed with the event in the same transaction on
Postgres and the sim store; written by the write-behind projection and gated by a
per-`(tenant, type, decl-set)` backfill watermark on Turso (no co-commit today).
Adding a declaration to an existing type triggers the same reconcile/backfill path as
declared keys.

### Sub-Decision 3: exact-scan kNN, ranked in the kernel (not the store)

The store returns candidate `(entity_id, vector)` rows for one
`(tenant, type, decl, model_tag)` partition in **deterministic entity-id order**; the
**kernel** computes the metric with f32 accumulation in that fixed order and keeps a
k-heap. Ranking in one place (not per backend) is what makes the result identical on
sim, Postgres, and Turso — the property the DST asserts. The candidate count charges
the existing `scan_candidate_budget`; an over-budget partition returns the same 413
(`QueryTooLarge`) contract as any other read. Ties break by entity id.

`@temper.score` is a closeness where higher = nearer for every metric: cosine
similarity, dot product, and **negative** L2 distance — so "ordered by score
descending, nearest first" holds uniformly.

### Sub-Decision 4: `Temper.Nearest` — a GET bound function

```
GET /tdata/DesignLanguages/Temper.Nearest(decl='taste',to='en-…',k=10)
GET /tdata/DesignLanguages/Temper.Nearest(decl='taste',vector='[…]',k=10,model='…')
GET /tdata/DesignLanguages/Temper.Nearest(decl='taste',to='en-…',k=10,filter='Status eq ''Published''')
```

Named parameters: `decl` (required), one of `to` (rank against another entity's vector;
that entity is excluded from its own results) or `vector` (raw query vector; `model`
required), `k`, optional `model` override, optional equality `filter` applied before
ranking. Response is the standard OData list shape ordered by score, each row carrying
`@temper.score`. The equality filter is applied by walking the full ranked candidate
list in score order and materializing + filtering + Cedar-`read`-authorizing lazily
until `k` rows are accepted — which is exactly "filter, then take top-k."

**Why a bound function** (RFC-0003 Q1): agents consume this as a tool call, and a
bound function's named parameters map 1:1 onto a tool schema; a query-option grammar
would make agents compose nested custom syntax inside a URL string where they fumble
quoting.

### Sub-Decision 5: governance and budget are unchanged

The read runs under the same Cedar entityset `list` + per-row `read` authorization, the
same tenant isolation, and the same budget accounting as every query-plane read. That
is the entire point of doing this in the kernel.

## Consequences

### Positive
- Semantic search is a governed, budgeted, deterministic platform primitive every
  tenant gets by declaring five lines of TOML.
- One implementation replaces per-consumer app-side cosine.

### Negative
- The kernel gains a vector index table per backend and a ranking path to maintain.
- v1 loads a partition's vectors per query (fine at ≤1k; ANN is the declared escape
  hatch when a partition approaches the budget).

### Risks
- A backend computing its own ranking would diverge — mitigated by ranking only in the
  kernel over a store-supplied, id-ordered candidate list.
- Turso lag: a just-written vector may not be indexed yet — the write-behind projection
  plus the backfill watermark bound this exactly as ADR-0153 bounds keyed absence.

### DST Compliance
- Ranking is pure: no clock, no randomness, no map-iteration dependence; f32
  accumulation in the store's fixed id order; ties broken by entity id.
- The sim store co-commits vector rows under the same lock as the journal, so a read
  reflects the journal deterministically. A DST drives seeded writes + `Nearest` reads
  and asserts identical ordering across all seeds.

## Correctness details (review hardening)

- **Deletion and cleared vectors.** A write for a vector-declaring type *reconciles*
  the entity's index rows: the store deletes all of the entity's rows, then inserts
  the current ones. A soft-deleted (`status = "Deleted"`) entity emits no rows, so it
  is purged even though its embedding field persists; a cleared vector/model property
  drops that path's rows. As defense in depth the `Temper.Nearest` walk also skips
  any `Deleted` body and `to='<deleted-id>'` returns 404, so a stale row (e.g. one
  written by a Turso write-behind that has not yet caught up) is never *served*.
- **Reference-entity authorization.** `to='<id>'` read-authorizes the reference with
  the same Cedar `read` gate a normal single read uses, before disclosing its
  existence, embedding, or the similarity ranking it seeds. Every ranked row is
  `read`-gated during materialization, so a denied row is skipped, never leaked.
- **Numeric safety.** Metric accumulation is done in f64 and the score is dropped if
  the narrowed f32 is not finite; the blob decoder rejects non-finite components. An
  overflowing or corrupt vector therefore declines rather than producing a `NaN`,
  which would otherwise sort ahead of every real score.
- **Budget.** `vector_candidates` applies `LIMIT budget+1` in the store, so an
  over-budget partition returns 413 without loading the whole partition. The ranked
  walk then materializes candidates one at a time until `k` are accepted; at the ≤1k
  target scale this N-at-most walk is microseconds, and a bounded batch materialization
  is the optimization if a partition ever approaches the budget.
- **Turso durability.** Turso maintains the index write-behind (event first, index
  follows), and that follow-up write is *retried* (same retry primitives as the event
  append) rather than a warn-once one-shot; on exhaustion it logs loudly and the
  partition lags until the next backfill reconcile. This is kept in the EventStore
  layer (where co-commit lives on Postgres) rather than routed through the
  field-index projection queue, because the queue is spec-agnostic (`QueryPlaneStore`)
  while vector parsing needs the spec, and routing it there would double-maintain on
  Postgres; the retry/remove substance is delivered in-layer.
- **Signature includes shape.** The backfill watermark records each path as
  `name:property:model_property:dims:metric`, so an in-place edit to `dims` (which
  makes every existing row the wrong length) — or to any other field — changes the
  signature and re-indexes the type, rather than silently leaving mismatched rows.
- **Surface.** `Temper.Nearest` is dispatched by its fully-qualified name, rejects
  unknown/duplicate arguments and any OData system query option (`$top`/`$select`/…),
  and is a kernel bound function discoverable via this ADR; it is **not** advertised in
  per-tenant `$metadata` (that is the producer's CSDL — kernel-side augmentation of the
  metadata pipeline is deferred).

## Non-Goals
- Embedding generation (stays in post-transition integrations/WASM).
- ANN / approximate indexes (a later declared `index = "hnsw"` opt-in).
- Consumer cutovers (Katagami "related", Aya retrieval) and deletion of interim
  app-side serving — a separate effort.
- Multimodal / dims-changing model swaps (a new declaration version + backfill).

## Alternatives Considered
1. **pgvector sidecar** — rejected as the end state (a second store to run and a
   parallel implementation); remains the documented escape hatch if kernel work stalls.
2. **Kernel-computed embeddings** — rejected permanently; a live API call in the write
   path breaks seed-reproducible verification.
3. **Automatic ANN switch at scale** — rejected; index behavior is a declared contract,
   not a silent heuristic (silent engine switches are how reads start lying).
4. **Ranking inside each store backend** — rejected; divergent f32 results across
   backends would make the read non-reproducible under DST.

## Rollback Policy
The declaration is additive and inert until a spec adds `[[vector]]`. Removing the
declaration stops maintenance; dropping `entity_vector_index` + the watermark rows
fully reverts the feature with no journal impact (the index is derived state).
