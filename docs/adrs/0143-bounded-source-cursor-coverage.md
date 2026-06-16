# ADR-0143: Reject Oversized Source-Cursor Proofs Before Catalog Coverage

## Status

Accepted

## Context

Filtered OData reads can fall back from native query-plane pages to a
source-cursor proof. Full-proof reads (`$filter`, `$orderby`, or `$count=true`)
must evaluate the candidate set to prove correctness. The source-cursor code
already rejected candidate sets above the bounded scan budget, but the caller
performed catalog coverage first. On large entity sets that meant a query could
load tens of thousands of catalog rows only to reject the read afterward.

## Decision

Before requesting catalog coverage for a source-cursor full-proof read, compare
the source candidate count with the query-plane scan budget. If it exceeds the
budget, return `QueryTooLarge` immediately with `FallbackCandidateBudget`
telemetry.

Unfiltered first-page reads keep their streaming behavior: they can still stop
after proving the requested page and do not need to reject solely because the
total source cursor is large.

## Consequences

- Oversized filtered reads fail fast instead of issuing broad catalog coverage
  materialization queries.
- Callers that need large filtered reads must use native pushdown, a narrower
  filter, or a smaller entity set.
- The query-plane budget remains the single bound for fallback proofs; coverage
  checks no longer bypass it.
