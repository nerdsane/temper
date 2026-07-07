# ADR-0158: Escaping untrusted input at the Cedar and ClickHouse boundaries

- Status: Accepted
- Date: 2026-07-07
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar authorization for agents
  - ADR-0005: Agent policy and audit layer
  - ADR-0039: Authz policy traceability
  - ARN-172 (Cedar policy injection), ARN-174 (ClickHouse SQL injection)
  - `crates/temper-authz/src/policy_gen.rs`
  - `crates/temper-observe/src/clickhouse.rs`

## Context

Two hand-rolled string builders splice agent-influenced values into a policy/query
language with incomplete escaping. Both originate from the same class of bug:
untrusted input concatenated into a structured target language without escaping the
full set of metacharacters that language honors.

### ARN-172 — Cedar policy injection

`generate_cedar_from_matrix` builds Cedar policy text with `format!`, interpolating
`agent_id`, `action`, `resource_type`, `resource_id`, `role`, `agent_type`, and
`session_id` directly into UID and string-condition positions with no escaping:

```rust
format!("principal == {}::\"{}\"", principal_kind, agent_id)
```

These values flow from `PendingDecision::from_denial` — the action/resource of the
request an agent tried and was denied, ultimately influenced by the spoofable
`x-temper-*` request headers. A value containing `"` or `\` either:

- breaks the generated `permit`, so the whole tenant policy set fails to re-parse in
  `reload_tenant_policies` (the generated policy is concatenated with the tenant's
  existing policies and parsed as one unit — one bad statement rejects the batch), or
- crafts a policy whose meaning differs from what the human approved. A resource id
  like `x") ; permit(principal, action, resource); //` turns a narrow, single-resource
  approval into a broad grant. This defeats the human approval gate.

The type-name positions (`principal_kind`, `resource_type`) are Cedar identifiers, not
string literals — `Order::"id"`. An identifier cannot be "escaped"; an injected `"` there
must be rejected, not encoded.

### ARN-174 — ClickHouse SQL injection

`interpolate_params` escapes string parameters by doubling single quotes only
(`s.replace('\'', "''")`). ClickHouse string literals also honor C-style backslash
escapes, so a value ending in `\` renders as `'...\'` where `\'` is an escaped quote,
leaving the literal unterminated and letting the next parameter execute as live SQL.
`ClickHouseStore` is the production query adapter; the Postgres/Turso/Redis stores use
real parameter binding and are not affected.

## Decision

Escape at both boundaries using the target language's own rules, and prefer the
language's typed constructors over hand-rolled string escaping wherever the crate
already depends on them.

### Sub-Decision 1: Cedar — build UIDs and literals via Cedar's own types

`temper-authz` already depends on `cedar-policy`. Instead of hand-rolling Cedar string
escaping (the exact mistake ARN-174 shows is easy to get wrong), use Cedar's own
rendering, which is guaranteed to round-trip through Cedar's parser:

- **UID positions** (`principal == T::"id"`, `action == Action::"id"`,
  `resource == T::"id"`): construct a `cedar_policy::EntityUid` from
  `EntityTypeName::from_str(type)` + `EntityId::new(id)` and render it. This validates
  the type name (rejecting injection in the identifier position) and escapes the id.
- **Bare type positions** (`principal is T`, `resource is T`): validate the type name
  with `EntityTypeName::from_str` and render its canonical form.
- **String-condition positions** (`context.role == "..."`, `context.agentType == "..."`,
  `context.sessionId == "..."`): render the literal as `"{}"` around
  `EntityId::escaped()`, which is Cedar's own string-escaping routine (the same one
  `EntityUid`'s `Display` uses).

Because the type name can be invalid, `generate_cedar_from_matrix` now returns
`Result<String, String>`. An invalid type name fails closed — no policy is generated,
and the error is surfaced to the caller (which already reports failures to the human
channel) rather than silently producing a wrong or broken policy.

**Why this approach**: it delegates all escaping to Cedar itself, so there is no
second, hand-maintained definition of "what Cedar strings look like" to drift from the
parser. Rejecting (not encoding) bad identifiers is the only correct option for the
type position.

### Sub-Decision 2: ClickHouse — escape backslash before quote

Escape the backslash first, then the single quote:

```rust
s.replace('\\', "\\\\").replace('\'', "''")
```

Order matters: escaping `\` first means the `\` we introduce for `'` is not
re-escaped. Doubling single quotes remains correct for ClickHouse. A value ending in
`\` now renders as `'...\\'` — a terminated literal containing a literal backslash —
so a following parameter can no longer break out into live SQL.

This keeps the existing single-pass, quote-and-placeholder-aware interpolation (which
already correctly leaves `$N` inside string literals intact); only the string
rendering changes. The longer-term option — ClickHouse's `param_<name>` HTTP binding —
is recorded as a non-goal below.

## Consequences

### Positive
- The human approval gate can no longer be bypassed by an injected resource id/action.
- A crafted id/action no longer breaks the whole tenant policy reload.
- ClickHouse string parameters are safe against backslash-terminated breakout.
- Cedar escaping is owned by Cedar, not duplicated in Temper.

### Negative
- `generate_cedar_from_matrix` is now fallible; callers must handle the error. This is
  a small, correct ripple — the alternative (silently sanitizing a type name) would
  change policy meaning.

### Risks
- If a legitimate entity type name is ever non-identifier-shaped, policy generation
  fails closed. Temper entity types are identifiers, so this is not expected in
  practice; the failure is loud and surfaced, not silent.

### DST Compliance
- `temper-observe` is not simulation-visible. `temper-authz` policy generation is pure
  string transformation with no clocks, RNG, or threads. No determinism concerns.

## Non-Goals
- Migrating `ClickHouseStore` to ClickHouse's native `param_<name>` HTTP parameter
  binding. That is a larger change to the query path; escaping fully closes the
  injection today and the binding migration can follow.
- Reworking the request-time UID construction in `crates/temper-authz/src/engine/mod.rs`,
  which uses the same `format!` + `EntityUid::from_str` shape but fails closed (a malformed
  parse denies the request) rather than emitting reusable policy text. Flagged as a
  follow-up.

## Alternatives Considered

1. **Hand-rolled Cedar string escaper** — a `escape_cedar_string` that replaces `\`
   and `"`. Rejected: it re-implements Cedar's string grammar in a second place that
   can drift from the parser, which is precisely the ARN-174 failure mode. Cedar's own
   `EntityId::escaped()` is authoritative and free.
2. **Reject non-identifier ids entirely** (in every position) — Rejected: ids are
   legitimately arbitrary strings (`order-123`, uuids, names with punctuation).
   Escaping is correct for id/string positions; rejection is only correct for the
   type-name identifier position.
3. **ClickHouse `param_<name>` binding now** — Correct long-term, but a broader change
   than the security fix needs; recorded as a non-goal / follow-up.
