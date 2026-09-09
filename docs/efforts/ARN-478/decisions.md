# Decisions & Tradeoffs

## Decision

An incomplete historical audit blocks contract activation.

Came up because latest values alone cannot prove historical set-once semantics. Source verification confirmed that the data-only create path does persist a `Created` event; data-only status alone is not evidence of missing history.

Options: infer history from latest values; introduce a new retroactive baseline policy; report incomplete and retain the old spec.

Chose explicit incomplete-audit rejection because the ADR already promises historical consistency and fail-closed activation; latest values cannot prove prior immutability.

Where: PR #411, docs/adrs/0156-immutable-typed-cross-entity-references.md, Sub-Decision 9.

## Review corrections within the accepted design

Decision: Require a compatible string key schema for the already-proposed SHA-256 identity, and assess audit completeness from actual history.

Came up because the first panel found that a 64-character hash cannot satisfy the usual `Edm.Guid` key schema, and the earlier audit wording could wrongly discount data-only creation events.

Options: change the proposed identity format; leave the metadata contradiction unresolved; clarify the existing hash contract and history evidence.

Chose the contract clarification because it preserves the proposed identity format and fail-closed audit without adding runtime implementation or a new migration mechanism. The data-only `Created` event counts as evidence; missing required history still blocks activation. The ADR status uses the template's plain Accepted value; its existing implementation-status paragraph remains explicit.

Where: PR #411, ADR-0156 Sub-Decisions 4 and 9; `crates/temper-server/src/state/entity_ops.rs`, `try_create_data_only_tenant_entity`; `test-fixtures/specs/model.csdl.xml`.

Scope constraint: This effort changes documentation only. Review requests for runtime implementation, broader verification redesign, or unrelated improvements do not expand this PR.
