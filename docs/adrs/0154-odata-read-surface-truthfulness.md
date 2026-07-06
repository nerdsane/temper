# ADR-0154: OData read-surface truthfulness — keyset continuation and projection parity

- Status: Accepted
- Date: 2026-07-04
- Deciders: Temper core maintainers
- Related:
  - ADR-0153: Declared composite key index (the read plane this builds on)
  - `crates/temper-server/src/odata/query_plane_read/` (read planner)
  - `crates/temper-odata/src/query/types.rs` (query-option parser)

## Context

Two ways the OData read surface lied to callers:

- **ARN-160 — silent truncation.** A list read truncated by the page size
  (default 100, or `$top` capped at `max_entities`) returned a full page with **no
  `@odata.nextLink`** and no count. Measured in production: `GET
  /tdata/DesignLanguages?$filter=Status eq 'Published'` returned exactly 100 rows
  while `$top=1000` returned 214 and `$count=true` reported 214. Per OData v4 a
  server that applies server-driven paging MUST include `@odata.nextLink` on a
  partial result. Agents concluded the catalog held 100 items. The kernel exposed
  no continuation the client could follow reliably; the Katagami UI had hand-rolled
  keyset pagination (`Id lt <cursor>` + `$orderby=Id desc`) to cope.

- **ARN-97 — projection changed membership.** A Published entity present in a
  canonical list-read was **absent under a `$select` projected list-read** of the
  same `$filter`. Historically the `$select` path was answered by a projection-only
  fast path that bypassed the coverage/reconcile machinery the canonical path uses,
  so an entity missing or stale in the projection could be dropped only under
  `$select`. That fast path is already dead (`#[cfg(test)]`), but the read still
  carried `$select` *through* the plan, leaving membership select-conditioned by
  construction.

## Decision

### 1. Keyset `$skiptoken` continuation over the read's canonical order

Add `$skiptoken` to `QueryOptions` and the parser. When a list read is truncated,
the server emits `@odata.nextLink` carrying an opaque, URL-safe `$skiptoken`; a
client echoes it back verbatim to fetch the next page.

The token is a **keyset cursor** over the read's **canonical order** = the request's
`$orderby` clauses followed by `entity_id` ascending as a total-order tiebreaker
(the id both the native SQL page and the in-memory sort already order by, so the
order is a strict total order over distinct entities). The token records the
ordering values of the last returned row; the next page keeps only rows that sort
**strictly after** it.

**Why keyset, not `$skip`/offset:** offset pagination duplicates or skips rows when
the set changes between pages, and the kernel's read budget makes deep offsets
expensive. A keyset cursor is stable under concurrent inserts/deletes and is what
the UI already reached for.

**Why express it as a `$filter` predicate:** the continuation is lowered to an
ordinary keyset `$filter` (row strictly after the cursor) plus the canonical
`$orderby`, then run through the **existing** plan — native pushdown narrows by the
lossless conjuncts, the in-memory re-check honors the keyset. No new storage
method, and it behaves identically on Postgres, Turso, and the sim store. Null
ordering matches the backends' `NULLS LAST` ascending / `NULLS FIRST` descending,
which is also what the in-memory sort produces.

Truncation is detected by fetching one row past the page (the read budget is given
one row of headroom so the probe row cannot itself trip a `413` at the boundary).

### 2. `$select` is a post-read projection, so it cannot change membership

Paging is layered in `read_entity_set_from_query_plane` around the core planner
`read_entity_set_page`. The wrapper strips `$select` from the underlying read (the
planner always materializes full bodies, so the cursor can read the ordering
properties) and applies the projection **after** the continuation cursor is taken.

A projected list-read is therefore, by construction, the canonical read's entity
set with a field projection applied — only the field shape differs. This closes
ARN-97 structurally: `$select` can no longer condition membership.

## Consequences

- Following `@odata.nextLink` from the first page to the last enumerates the full
  result set with no duplicates or gaps, proven by tests on Turso and the sim
  store (union of pages == one big `$top` read), for unfiltered, filtered, and
  `$orderby` reads.
- Deep pagination is still bounded by the existing scan-candidate budget
  (`max_entities * 10`): resuming past the budget returns `413 QueryTooLarge`
  rather than an unbounded scan — the same contract as a large filtered/counted
  read. A future optimization can push the keyset predicate into SQL to make deep
  pages cheap; today they re-materialize within the budget, matching the cost of
  the pre-existing offset scan.
- A malformed or mismatched `$skiptoken` returns `400 InvalidSkipToken`; clients
  must follow the link verbatim and never construct a token.
- `$skip` still works and composes with page one; the emitted `nextLink` drops
  `$skip` in favor of the keyset token.

## Follow-ups

- Remove the now-unreachable `$select` projection-only body path
  (`catalog_row_to_selected_entity_body` and the `selected_catalog_fields`
  materialization argument) so it cannot be re-wired; the coverage presence check
  keeps its narrow `entity_id`-only projected load. Left out of this change to keep
  the diff focused on read-surface behavior.
