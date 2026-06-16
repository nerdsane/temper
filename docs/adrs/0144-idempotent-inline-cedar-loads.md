# ADR-0144: Inline Cedar Policy Loads Are Idempotent

## Status

Accepted

## Context

`/api/specs/load-inline` can submit bundled Cedar policy text alongside inline
specs. The loader appended that text to the tenant's in-memory policy bundle on
every successful load. Repeated app reconcile or server restart cycles could
therefore duplicate the same policy text many times, increasing Cedar parse and
authorization cost without adding new policy semantics.

OS app install already treats bundled policies as idempotent by skipping policy
text that is already present. Inline spec loading needs the same contract.

## Decision

When inline Cedar text is valid and non-empty, merge it into the tenant policy
text only if the exact trimmed bundle is not already present. Preserve existing
distinct policy text and separate newly-added policy bundles with one newline.

## Consequences

- Repeated inline app/spec loads no longer grow tenant policy text with duplicate
  policy bundles.
- Existing distinct tenant policy text is preserved.
- This is an in-memory idempotence guard for the inline endpoint; durable
  granular policy row storage remains the preferred long-term app-policy model.
