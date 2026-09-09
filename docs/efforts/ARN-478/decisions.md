# Decisions & Tradeoffs

## Decision

An incomplete historical audit blocks contract activation.

Came up because current data-only records can lack history needed to prove set-once semantics.

Options: infer history from latest values; introduce a new retroactive baseline policy; report incomplete and retain the old spec.

Chose explicit incomplete-audit rejection because the ADR already promises historical consistency and fail-closed activation; latest values cannot prove prior immutability.

Where: PR #411, docs/adrs/0156-immutable-typed-cross-entity-references.md, Sub-Decision 9.
