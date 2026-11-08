# Integrations and webhooks

## Sub-features
Three distinct mechanisms - keep them separate:
- **Outbound integrations** declared in specs as `[[action.triggers]]` (`kind = "wasm"` / `"adapter"`; `kind = "webhook"` is refused at load, ADR-0182), plus deployment `webhooks.toml` keyed on action names, fired post-transition (outbox pattern). `[[integration]]` blocks and `trigger`/`emit` effects are retired (ADR-0180) and fail to load.
- **Inbound signed webhooks** (`/webhooks/{tenant}/{*path}`, any method, the provider's own signature scheme, fail-closed).
- **WASM-backed HTTP endpoints** (ADR-0069, inbound, `HttpEndpoint` entities).

## How to get to it (user POV)
A transition can call an external service; an external service can call in over a signed webhook to dispatch an action; a spec can expose its own HTTP route backed by a WASM module.

## Driving it

**Outbound** - declare a trigger on an action (`[[action.triggers]]` with `kind = "wasm"`/`"adapter"` and `config = { url = ..., method = ... }`), or a `[[webhook]]` entry in the app's `webhooks.toml` with `url` and `actions = ["<ActionName>"]` (each action emits its own name implicitly; `temper-server/src/webhooks/dispatcher.rs`), invoke the triggering action, and watch for the async request at the endpoint. A `kind = "webhook"` trigger is refused when the spec loads (ADR-0182, `temper-spec/src/automaton/validate/triggers.rs`): nothing sends it. It is fire-and-forget post-commit: the transition completes regardless, and the observable effect is the outbound request plus a `tracing` success/failure line. Beyond that, behavior depends on the wiring, so verify only what your spec actually exercises: retry/backoff and a dead-letter queue exist in the platform integration engine (`crates/temper-platform/src/integration/`, delivery core `temper_runtime::webhook::deliver_webhook`), and a follow-up action is dispatched only when the path returns `callback_params` and the integration declares an `on_success`/adapter mapping. Do not assume retries/DLQ/callbacks for a plain webhook that declares none.

**Inbound signed webhook** - declare `[[webhook]]` with `path`, `action`, `hmac_secret` (required, supports `{secret:key}`), `signature` (`kernel` default, `hmac`, `github`, `stripe`, `standard_webhooks`, `token`), `hmac_header` for `kernel`/`hmac`/`token`, and `entity_id` / `[webhook.extract]` sources (`query.<name>`, `header.<name>`, `body`, `body.<path>`). Then sign the request the way the provider would:
```bash
# stripe: Stripe-Signature: t=<unix>,v1=<hex HMAC(secret, "<t>.<body>")>, t within 300 s of the server clock
# github: X-Hub-Signature-256: sha256=<hex HMAC(secret, body)>
# standard_webhooks: webhook-id, webhook-timestamp, webhook-signature: v1,<base64 HMAC(base64 key after whsec_, "<id>.<ts>.<body>")>
# kernel: <hmac_header>: <hex HMAC(secret, "<METHOD>\n<path-and-query as received>\n<body>")>,
#   where the path is the FULL /webhooks/<tenant>/<path>?<query> - not just /<path>.
curl -sS -X POST "http://localhost:3600/webhooks/default/<path>" \
  -H "Stripe-Signature: t=$T,v1=$SIG" -H 'content-type: application/json' -d '<body>'
```

## What proves it
- Outbound: the external endpoint receives the JSON envelope (`tenant, entity_type, entity_id, trigger_action, trigger_params, entity_state`) and the `tracing` success line fires. If (and only if) the integration wires a callback, `callback_params` re-enter as the mapped action (read the entity back). Retry/DLQ apply only on the platform-engine path.
- Inbound: the flow is signature admission -> reading the entity id and params -> Cedar authorization -> dispatch (`receiver.rs` `admit_webhook`, `read_action_input`, `authorize_with_context`, `dispatch_tenant_action`). A correctly signed AND Cedar-permitted request dispatches the configured action with the params read from the body, headers or query (entity moves, fields set); an unsigned, bad-signature or stale-timestamp request returns 401 before the body is read; a signed-but-denied request returns 403 without dispatching. All three are the proof.

## Gotchas
- A webhook without `hmac_secret` fails to load. At request time a missing vault, an unresolved `{secret:...}`, a missing or malformed signature header, or a timestamp more than 300 s from the server clock all return 401, with the reason in the `webhook signature refused` log line. Only the `kernel` scheme covers the method and the FULL path-and-query; the provider schemes sign the body (and their timestamp). The body is bounded only by the router's request-body limit (axum `DefaultBodyLimit`, 413 over). A body source on a non-JSON body returns 400.
- Outbound webhook/platform dispatch supports only POST/PUT (other verbs fall through to POST); the generic HTTP *adapter* honors arbitrary methods. Timeouts differ across paths (platform 5 s, shared runtime core 10 s).
- Triggers never change the state machine: a broken trigger never fails the transition - failures land in logs/DLQ (or its `on_failure` action). Verification sees triggers only through `on_success`/`on_failure` and `liveness = "required"`.
- WASM outbound calls go through `AuthorizedWasmHost`, which Cedar-authorizes every call by domain (userinfo stripped to block SSRF) - there is no static host allowlist, the gate decides.
- `HttpEndpoint` routes cannot shadow reserved namespaces (`/tdata`, `/webhooks`, `/api`, `/observe`, `/_admin`, `/_internal`); each carries its own fuel/memory/timeout/response caps.
- Discipline: one integration = one concern; a module fired by a transition must not itself dispatch transitions (see wasm-integration.md) - `on_success`/`on_failure` are how a result re-enters as an action.
