# ADR-0164: Spec-Declared Webhook Integrations Execute

## Status

Accepted (2026-07-14)

(Numbered 0164: 0156–0163 are claimed by concurrently open arena branches.)

## Context

An IOA spec can declare `[[integration]]` blocks. `type = "webhook"` is the
DEFAULT integration type — yet the runtime executed only `type = "wasm"`
integrations (`dispatch_wasm_integrations_internal` filters on `"wasm"`),
and the `WebhookDispatcher` fires only from the server-level
`webhooks.toml`. A developer's declared webhook parsed, passed the
verification cascade ("integrations are metadata only"), deployed — and
never did anything (ARN-227). Silently accepted configuration that does
nothing is the same failure class as a silently swallowed error.

## Decision

1. **Webhook integrations fire post-dispatch**, from the same hook that
   fires `webhooks.toml` webhooks (`fire_webhooks`), for successful actions
   only. An integration fires when its `trigger` equals the action name or
   any custom effect the transition produced — a superset of the wasm
   path's trigger semantics (wasm fires only on custom effects).
2. **Config contract — matches what the parser writes**, including the
   records it synthesizes from `[[action.triggers]]` webhook blocks
   (ADR-0046): `url` is required (missing url is a runtime warn — see
   residuals); `method` defaults to POST; `body_template` (the synth key)
   or `payload_template` becomes the request body, with `${...}`
   trajectory variables and `${field}` entity-field placeholders expanded
   (default body: a JSON object of the transition); `header.{Name}` keys
   are sent as HTTP header `Name`, with `{secret:key}` values resolved
   through the same `resolve_secret_templates` pass the wasm and adapter
   integration paths use — a header whose value still carries an
   unresolved secret template is dropped with a warning rather than leaked
   to the remote host. Any other config key is ignored (debug-logged),
   never sent.
3. **Fire-and-forget**: dispatched on a shared client held by
   `ServerState` (present regardless of whether `webhooks.toml` is
   configured), spawned off the action path — webhook latency or failure
   never blocks or fails the action, exactly like `webhooks.toml`
   dispatch. Failures are logged with the integration name and url.

## Consequences

- Declared webhook integrations now do what the spec says. The `[[webhook]]`
  (inbound receiver) and `webhooks.toml` (server-operator config) paths are
  unchanged.
- Delivery is at-most-once with no retries or ordering guarantees — the
  same contract `webhooks.toml` webhooks have always had. Systems needing
  guaranteed delivery need the outbox pattern; that is an explicit
  non-goal here.
- Outbound URLs come from specs, which are developer-approved design-time
  artifacts gated by the verification cascade — the same trust level as
  `webhooks.toml` (operator-authored). No new trust boundary is crossed.

## Residuals

- **L0 does not require `url` on webhook integrations.** A webhook
  integration without a url is still accepted at verification and warns at
  runtime. The verification-cascade rule ("webhook integrations must carry
  a url") is the follow-up — queued for Linear with the other L0 gaps.
- Wasm-only fields (`on_success`/`on_failure`) are ignored on webhook
  integrations rather than rejected; same L0 follow-up. (Trigger-synthesized
  records can carry them — the fire-and-forget webhook path does not
  dispatch follow-up actions; systems needing that use `kind = "wasm"`
  triggers.)

## Alternatives Considered

- **Rejecting `type = "webhook"` at L0 until supported**: honest but
  wrong-way — the type is documented, is the default, and the runtime
  machinery (dispatcher, template expansion) already existed for
  `webhooks.toml`. Executing them is less code than rejecting them well.
- **Blocking dispatch with on_success/on_failure like wasm integrations**:
  changes action latency semantics and couples state transitions to
  external availability; `webhooks.toml` fire-and-forget is the
  established webhook contract.
