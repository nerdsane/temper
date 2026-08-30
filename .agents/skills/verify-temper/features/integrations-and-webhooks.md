# Integrations and webhooks

## Sub-features
Three distinct mechanisms - keep them separate:
- **Outbound integrations** declared in specs (`[[integration]]` / `[[action.triggers]]`), fired post-transition (outbox pattern).
- **Inbound signed webhooks** (`/webhooks/{tenant}/{*path}`, HMAC, fail-closed).
- **WASM-backed HTTP endpoints** (ADR-0069, inbound, `HttpEndpoint` entities).

## How to get to it (user POV)
A transition can call an external service; an external service can call in over a signed webhook to dispatch an action; a spec can expose its own HTTP route backed by a WASM module.

## Driving it

**Outbound** - declare an integration on an action (`type="webhook"`, `url`, `trigger=<action>`, or an `ActionTrigger` of `kind="webhook"` with `url`+`method`), invoke the triggering action, and watch for the async POST at the endpoint. It is fire-and-forget post-commit: the transition completes regardless, and the observable effect is the outbound request plus a `tracing` success/failure line. Beyond that, behavior depends on the wiring, so verify only what your spec actually exercises: retry/backoff and a dead-letter queue exist in the platform integration engine (`crates/temper-platform/src/integration/`, delivery core `temper_runtime::webhook::deliver_webhook`), and a follow-up action is dispatched only when the path returns `callback_params` and the integration declares an `on_success`/adapter mapping. Do not assume retries/DLQ/callbacks for a plain webhook that declares none.

**Inbound signed webhook** - declare `[[webhook]]` with `path`, `action`, `hmac_secret` (supports `{secret:key}`), `hmac_header`, then:
```bash
# sig = HMAC_SHA256(secret, "POST\n/<path>?<query>\n<body>"), hex, optional "sha256=" prefix
curl -sS -X POST "http://localhost:3600/webhooks/default/<path>?entity_id=<id>" \
  -H "<hmac_header>: sha256=<hex>" -d '<body>'
```

## What proves it
- Outbound: the external endpoint receives the JSON envelope (`tenant, entity_type, entity_id, trigger_action, trigger_params, entity_state`) and the `tracing` success line fires. If (and only if) the integration wires a callback, `callback_params` re-enter as the mapped action (read the entity back). Retry/DLQ apply only on the platform-engine path.
- Inbound: a correctly signed request dispatches the configured action (entity moves); an unsigned/bad-sig request returns 401 without dispatching.

## Gotchas
- Inbound HMAC is mandatory and fail-closed: missing secret/header/vault or an unresolved `{secret:...}` all return 401. The signature covers method + full path-and-query + body, not the body alone. Body hard cap 64 KB (413 over).
- Outbound webhook/platform dispatch supports only POST/PUT (other verbs fall through to POST); the generic HTTP *adapter* honors arbitrary methods. Timeouts differ across paths (platform 5 s, shared runtime core 10 s).
- Integrations are metadata-only: a broken integration never fails the state machine or verification - failures land in logs/DLQ.
- WASM outbound calls go through `AuthorizedWasmHost`, which Cedar-authorizes every call by domain (userinfo stripped to block SSRF) - there is no static host allowlist, the gate decides.
- `HttpEndpoint` routes cannot shadow reserved namespaces (`/tdata`, `/webhooks`, `/api`, `/observe`, `/_admin`, `/_internal`); each carries its own fuel/memory/timeout/response caps.
- Discipline: one integration = one concern; a module fired by a transition must not itself dispatch transitions (see wasm-integration.md) - `on_success`/`on_failure` are how a result re-enters as an action.
