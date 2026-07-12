# ADR-0163: Typed query parameters at observability-store boundaries

- Status: Proposed
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ARN-174: ClickHouse SQL injection through string interpolation
  - `crates/temper-observe/src/clickhouse.rs`
  - `crates/temper-store-postgres/src/query_page.rs`
  - `crates/temper-store-turso/src/store/query_page.rs`

## Context

`ClickHouseStore` expands the store-wide `$N` placeholder convention by rendering
values into SQL text. Its string renderer doubles single quotes but does not model
ClickHouse's backslash escapes. A value ending in a backslash can therefore escape
the generated closing quote and allow later input to become SQL syntax.

The same audit found two narrower manual literal builders for dynamic OData order
field names in PostgreSQL and Turso. Keeping any security-sensitive SQL literal
renderer would preserve the implementation class that caused ARN-174.

## Decision

ClickHouse queries will translate trusted `$N` placeholders into ClickHouse typed
placeholders such as `{p1:String}`. The SQL remains in the multipart `query` field;
each value is sent separately as `param_pN`. Parameter text never becomes SQL source.

The translator is a single-pass scanner over trusted query templates. It recognizes
single-, double-, and backtick-quoted regions plus line and block comments, and only
rewrites placeholders in SQL code. Missing and zero-indexed parameters fail locally.
`SqlParam::Null` remains the trusted SQL token `NULL`, because the cross-store enum
does not carry the target type required for `Nullable(T)`.

ClickHouse's server project documents this exact multipart protocol (`query` plus
`param_pN` fields) in [ClickHouse/ClickHouse#8842](https://github.com/ClickHouse/ClickHouse/issues/8842),
including a working server example. The official Java client tracks and ships the
same body-based parameter transport in
[ClickHouse/clickhouse-java#2324](https://github.com/ClickHouse/clickhouse-java/issues/2324).

PostgreSQL will bind dynamic JSONB member names. Turso will use `json_each(fields)`
and bind the exact member key rather than constructing a JSON path literal.

**Why this approach**: native typed parameters delegate value encoding to the
database protocol and eliminate a security-critical escaper instead of extending it.

## Rollout Plan

1. Replace interpolation and dynamic field-name literals in the three adapters.
2. Validate the real ClickHouse multipart request against a local mock HTTP endpoint.
3. Verify against a live ClickHouse service before merge or record that external gate
   explicitly when no service is available locally.

## Readiness Gates

- Attacker-controlled parameter bytes do not occur in generated SQL.
- The real HTTP request carries values only in `param_pN` multipart fields.
- Quoted/comment placeholders are not rewritten.
- PostgreSQL and Turso dynamic order fields are bound.
- Formatting, focused tests, independent review, and live ClickHouse verification pass.

## Consequences

### Positive

- The reported quote/backslash exploit and the entire manual-value-escaping class are removed.
- Values stay out of routine query URLs and SQL logs.
- All three affected adapters share the same rule: data is bound, never rendered as SQL.

### Negative

- ClickHouse request construction becomes multipart rather than a raw SQL body.
- The placeholder scanner must track SQL lexical regions in trusted templates.

### Risks

- ClickHouse resolves response format before multipart parsing, so `default_format` remains
  a fixed URL query parameter while SQL and values stay in the request body.
- Null binding remains untyped until `SqlParam` carries provider-specific nullable types.

### DST Compliance

- These adapters are not simulation-visible. The transformation is deterministic and pure.

## Non-Goals

- Changing the universal store placeholder convention.
- Adding provider-specific types to `SqlParam`.
- Treating identifiers as values; trusted SQL structure remains in query templates.

## Alternatives Considered

1. **Escape backslashes before quotes** — Rejected because it retains a second ClickHouse
   string grammar implementation and leaves future escape drift possible.
2. **Put query parameters in the URL** — Rejected because values would appear in routine
   proxy access logs and query length would be constrained by URL limits.

## Rollback Policy

Reverting restores the vulnerable interpolation path and is not permitted after release.
If multipart compatibility fails, replace it with ClickHouse's equivalent typed parameter
transport; do not restore value interpolation.
