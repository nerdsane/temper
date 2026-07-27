# ADR-0158: Typed construction at Cedar and ClickHouse boundaries

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
  - `crates/temper-store-postgres/src/query_page.rs`
  - `crates/temper-store-turso/src/store/query_page.rs`

## Context

Two hand-rolled string builders splice agent-influenced values into a policy/query
language with incomplete escaping. Both originate from the same class of bug:
untrusted input is concatenated into a structured target language instead of being
passed through that language's typed construction or parameter API.

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

Remove untrusted string interpolation at both boundaries. Use Cedar's typed value
constructors for generated policy fragments and ClickHouse's typed HTTP query
parameters for query values.

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
- **String-condition positions** (`principal.role == "..."`,
  `principal.agent_type == "..."`, `context.sessionId == "..."`): render the literal as `"{}"` around
  `EntityId::escaped()`, which is Cedar's own string-escaping routine (the same one
  `EntityUid`'s `Display` uses).

Because the type name can be invalid, `generate_cedar_from_matrix` now returns
`Result<String, String>`. An invalid type name fails closed — no policy is generated,
and the error is surfaced to the caller (which already reports failures to the human
channel) rather than silently producing a wrong or broken policy.

The generator itself also calls `validate_policy_scope_matrix`. Validation at a UI or
caller is not sufficient for a security boundary: without the required role,
agent-type, or session companion value, release builds must return an error rather
than emit a broader permit after a debug assertion disappears.

Scope dimensions compose monotonically: `all_actions_on_type` always emits the
approved resource-type constraint, even when the resource dimension is
`any_resource`. A broader dimension cannot erase the boundary promised by a
narrower one.

Decision policy rows use an atomic insert-if-absent operation in every durable
backend. The `(tenant, decision:<id>)` key is therefore immutable even when two
server processes approve concurrently: only one insert wins, and the loser
must verify the exact stored content instead of overwriting it.
The generic policy API cannot create rows in the reserved `decision:` namespace
or edit their Cedar text. Explicit disable/delete operations remain the
revocation mechanism.

Approval also requires the authenticated principal kind captured on the
pending decision. Legacy or corrupt rows without that authority fact fail
closed instead of silently defaulting to `Agent`, which could authorize a
different principal namespace sharing the same identifier.

Role and agent-type scopes read canonical principal attributes and require
`principal.agentTypeVerified == true`. The authorization engine loads arbitrary
principal extension attributes before canonical identity attributes, so extensions
cannot replace the canonical id, role, type, or verified bit. Resource fields remain
available as `resource.<field>`, but authority names (`role`, `actingFor`, `agentId`,
`agentType`, `agentTypeVerified`, and `sessionId`) are not copied into the legacy flat
`context` namespace. This prevents an OData entity body from satisfying an identity or
session condition. If principal and resource UIDs coincide, canonical principal facts
win any attribute-name collision on the merged Cedar entity.

Security-sensitive policy construction has one implementation in `temper-authz` and
one decision-id-keyed installation path in `temper-server`. The REST approval boundary
generates the policy once from the durable `PendingDecision`, validates the full named
tenant set, writes exactly one immutable `decision:{pending-decision-id}` row, re-reads
that row, and activates the durable named set. A retry accepts only byte-identical,
enabled content under that id; it cannot replace a previous approval with a wider
matrix. Activation failure rolls back a newly-created row and restores the prior set.

`GovernanceDecision.Approve` carries a serialized receipt in its existing string
`scope` field. The receipt binds the pending-decision id, GovernanceDecision actor id,
principal kind, and complete scope matrix. Its `GenerateCedarPolicy` custom effect is
now verification-only: it reproduces the policy through the canonical generator and
requires structural Cedar AST equality with exactly one active policy. It never
appends, persists, or reloads another policy. This removes the prior REST-plus-hook
duplicate in which a session-only approval was followed by a second permanent
`narrow` permit.

Custom effects remain ordered and stop at the first failure. A failure returns
`success=false`, prevents later callbacks from running, and leaves dispatcher effects
unapplied so the same decision-scoped idempotency key can retry them. Callback target
actions are awaited and use a stable GovernanceDecision/status idempotency key.
Direct/internal dispatch now checks the completed-effects cache before asking the actor:
an incomplete sequence replays its cached effects, while a completed sequence returns
without applying the callback transition or its downstream effects twice.

Actor idempotency is durable across process restart. Each committed event records its
ordered custom-effect names, and the bounded per-entity idempotency map stores the
event sequence, exact action, a deterministic SHA-256 digest of canonical JSON action
parameters, and those effects. A duplicate therefore returns the committed effect
receipt without evaluating the action against mutable terminal state. Reusing a key
for another action or parameter payload fails closed. Replay reconstructs receipts for
older events from the historical `from_status` and action without re-running guards;
legacy sequence-only snapshots remain readable but cannot invent missing effects.

The generated GovernanceDecision automaton commits `Approved` before custom effects.
Consequently arbitrary internal callers that bypass the REST preinstallation boundary
can still leave the GovernanceDecision entity terminal while the verification effect
reports failure; the callback is not emitted and the failure is explicit/retryable,
but event-state rollback is impossible post-commit. Full atomicity for that unsupported
direct path requires a generated two-phase spec (`Pending -> Installing -> Approved`)
or a generated pre-transition effect. This ADR does not hand-edit the IOA source. The
normal REST path makes the post-commit verifier non-fallible by preinstalling and
checking the exact receipt before dispatch.

**Why this approach**: it delegates all escaping to Cedar itself, so there is no
second, hand-maintained definition of "what Cedar strings look like" to drift from the
parser. Rejecting (not encoding) bad identifiers is the only correct option for the
type position.

### Sub-Decision 2: ClickHouse — bind typed HTTP query parameters

Delete value rendering from `ClickHouseStore`. The adapter translates the universal
store's positional `$N` placeholders into ClickHouse placeholders such as
`{p1:String}`. It sends the SQL as the multipart `query` field and each value as a
separate `param_pN` field. ClickHouse substitutes these values at the query AST rather
than treating them as SQL source text. `default_format=JSONEachRow` stays in the URL:
ClickHouse resolves the response format before it parses multipart fields. Query text
and values remain in the POST body, avoiding URL-length limits and preventing query
values from appearing in routine proxy access logs.

The parameter type follows the `SqlParam` variant:

- `String` -> `String`
- `Int` -> `Int64`
- `Float` -> `Float64`
- `Bool` -> `UInt8` with value `0` or `1`
- `Null` -> the trusted SQL token `NULL`, because the cross-provider `SqlParam::Null`
  does not carry the target type required by a ClickHouse `Nullable(T)` placeholder

The placeholder translator remains a single pass so it cannot rescan parameter
values. It recognizes quoted SQL regions and comments so `$N` text in a literal,
identifier, or comment remains source text rather than becoming a binding. Missing
parameters fail locally before any HTTP request. Values are never escaped or appended
to the SQL body.

The same audit found dynamic `$orderby` field names rendered as SQL string literals in
the Postgres and Turso query-page adapters. Both adapters now allocate native bind
parameters after the filter parameters and before limit/offset. Postgres binds the
JSONB key directly. Turso traverses object members with `json_each(fields)` and binds
the exact member key in `WHERE key = ?N`; this avoids constructing a JSON path at all.
Neither adapter maintains an SQL literal escaper for order-field input.

### Sub-Decision 3: Cedar request UIDs use the same typed constructors

Request-time authorization uses the same `EntityTypeName`, `EntityId`, and `EntityUid`
constructors. The prior `format!`-then-parse path denied malformed text rather than
injecting policy, but it was a duplicate construction strategy and rejected valid IDs
containing Cedar metacharacters. There is now no request-time UID string builder.

The CLI's fallback policy builder validates spec-derived entity types through the
same exported `render_cedar_entity_type` boundary before placing a bare type into
policy structure. Parsing the final combined policy remains a defense-in-depth check,
not the first point at which an injected type is rejected.

## Consequences

### Positive
- The human approval gate can no longer be bypassed by an injected resource id/action.
- A crafted id/action no longer breaks the whole tenant policy reload.
- ClickHouse values never become SQL source text, closing the entire escaping class
  rather than only the known quote/backslash exploit.
- Dynamic order-field values are bound in all durable query-page adapters; fixing the
  ClickHouse exploit does not leave parallel manual SQL literal builders elsewhere.
- Cedar escaping is owned by Cedar, not duplicated in Temper.

### Negative
- `generate_cedar_from_matrix` is now fallible; callers must handle the error. This is
  a small, correct ripple — the alternative (silently sanitizing a type name) would
  change policy meaning.
- The ClickHouse adapter must translate the store-wide `$N` convention into typed
  ClickHouse placeholders. This is bounded linear work over trusted query templates.

### Risks
- If a legitimate entity type name is ever non-identifier-shaped, policy generation
  fails closed. Temper entity types are identifiers, so this is not expected in
  practice; the failure is loud and surfaced, not silent.

### DST Compliance
- `temper-observe` is not simulation-visible. `temper-authz` policy generation is pure
  string transformation with no clocks, RNG, or threads. The durable idempotency
  parameter digest recursively sorts JSON object keys and uses only committed event
  inputs, so restart replay is deterministic. No wall clock or random source is added.

## Alternatives Considered

1. **Hand-rolled Cedar string escaper** — a `escape_cedar_string` that replaces `\`
   and `"`. Rejected: it re-implements Cedar's string grammar in a second place that
   can drift from the parser, which is precisely the ARN-174 failure mode. Cedar's own
   `EntityId::escaped()` is authoritative and free.
2. **Reject non-identifier ids entirely** (in every position) — Rejected: ids are
   legitimately arbitrary strings (`order-123`, uuids, names with punctuation).
   Escaping is correct for id/string positions; rejection is only correct for the
   type-name identifier position.
3. **Escape ClickHouse backslashes before quotes** — Rejected: it closes the reported
   exploit but keeps a security-critical SQL literal renderer and placeholder scanner
   in Temper. Native typed parameters remove the vulnerable implementation class with
   a small, testable adapter change.
