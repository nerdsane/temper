# ADR-0182: Webhooks verify the provider's own signature and read the body

- Status: Proposed
- Date: 2026-10-22
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Class-A auth edge (webhook ingress is its separate Class B boundary)
  - ADR-0181: Strict typed spec reading (`entity_id = "query.<name>"`)
  - ADR-0046: Unified action triggers (the `kind = "webhook"` known gap)
  - `crates/temper-server/src/webhooks/receiver.rs`, `webhooks/signature.rs`
  - `crates/temper-spec/src/automaton/types/webhook.rs`, `validate/webhooks.rs`

## Context

`[[webhook]]` is how another system calls an app: a request to
`/webhooks/{tenant}/{path}` becomes one action on one entity. Four things kept
it from working for the systems that actually send webhooks:

1. **Only Temper's own signature.** The receiver checked a hex HMAC of
   `METHOD\nPATH?QUERY\nBODY`. Stripe signs `<t>.<body>`, GitHub signs the body,
   Standard Webhooks (Svix) signs `<id>.<timestamp>.<body>`. None of them can
   produce Temper's format, so none could call an app directly; Katagami built
   its own verifier in front of the kernel instead.
2. **Only the query string.** The entity id and every parameter had to be
   `query.<name>`. Providers put the ids in the JSON body
   (`data.object.client_reference_id`), so the body never reached the action.
3. **Invented bounds.** A 64 KiB body cap with no reason given, and only GET and
   POST routed (the edge admitted only those two as well).
4. **Declarations that install and do nothing.** A webhook without
   `hmac_secret` loaded and answered every request 401. A `kind = "webhook"`
   trigger loaded and never sent its request (ADR-0046 known gap).

The signature requirement and the Cedar permit are a real boundary. The
endpoint is public; without a signature anyone could post a provider's events,
such as a forged "payment succeeded". They stay.

## Decision

### Sub-Decision 1: The declaration names the provider's scheme

`signature` chooses how the request was signed; every HMAC scheme is
HMAC-SHA256 keyed by `hmac_secret`, and MACs are compared in constant time.

| `signature` | Header | Signed |
|---|---|---|
| `kernel` (default) | `hmac_header`, hex | `METHOD\nPATH?QUERY\nBODY` (unchanged) |
| `hmac` | `hmac_header`, hex or base64, optional `sha256=` | the raw body |
| `github` | `X-Hub-Signature-256: sha256=<hex>` | the raw body |
| `stripe` | `Stripe-Signature: t=..,v1=..` (only `v1` counts) | `<t>.<body>` |
| `standard_webhooks` | `webhook-id/-timestamp/-signature` or `svix-*`, `v1,<base64>` | `<id>.<timestamp>.<body>`, key = base64 after `whsec_` |
| `token` | `hmac_header` carries the secret itself | nothing (GitLab, Datadog custom headers) |

`stripe` and `standard_webhooks` refuse a timestamp more than 300 seconds from
the server clock, either way. That is the default of Stripe's libraries and of
the Standard Webhooks reference libraries, and it is what stops a captured
request being replayed later. `token` is weaker than a signature (it covers
neither the body nor the time) and exists because some senders cannot sign.

Existing declarations, which name no `signature`, keep the `kernel` scheme.

### Sub-Decision 2: The body reaches the action

`entity_id` and each `[webhook.extract]` value is a source: `query.<name>`,
`header.<name>`, `body` (the whole JSON body) or `body.<path>` (a dot path; a
number indexes an array). Body values keep their JSON type; a numeric id
becomes text. Nothing in the request is read until its signature checks out.
A body source on a body that is not JSON answers 400, as does a missing id.

### Sub-Decision 3: No invented bounds

The body is bounded only by the router's request-body limit
(`DefaultBodyLimit`), which answers 413 itself. Any method reaches the receiver
and the edge admits any method under `/webhooks/`, because the signature, not
the verb, admits a request. `method` is now optional: a webhook that names one
answers only it (405 otherwise); one that names none answers any.

### Sub-Decision 4: Fire or refuse at load

A spec loads only if each webhook can fire: it names a declared action, a
non-empty `hmac_secret`, `hmac_header` exactly when its scheme takes one, and
readable sources. A `kind = "webhook"` trigger is refused with a pointer to
`kind = "wasm"` with `module = "http_fetch"`, which does send. Making it fire
is a separate design (outbound egress governance, body templates), not a
consequence of the receiver.

## Consequences

### Positive
- A Stripe, GitHub, Meta (same header as GitHub), Shopify, Svix-signed
  (Clerk, Resend, Tensorlake, Composio) or GitLab sender can call an app
  directly, and the event's own fields become the action's parameters.
- A declaration that could never fire fails when it is installed, with the
  reason, instead of in a provider's delivery log.

### Negative
- A spec stored before this change with an unsigned webhook or a
  `kind = "webhook"` trigger no longer loads. No such spec exists in Temper,
  TemperPaw or Katagami (Katagami's app builder already refuses `[[webhook]]`).
- An OAuth redirect cannot be signed by its provider, so it still needs a
  relay that signs with the kernel scheme.

### Risks
- Replay inside the 300-second window is possible for any scheme, and always
  for schemes without a timestamp. The action's own state machine must refuse
  a repeated event (move only from the state the event finds).

### DST Compliance
- The clock is `sim_now()`; signature checks are pure functions of the request,
  the secret and that time.

## Non-Goals
- Slack's `v0:` and Twilio's URL-based schemes; asymmetric Standard Webhooks
  (`v1a`) signatures.
- Changing the server-wide request-body limit or HTTP route management.
- Event-type filtering in the receiver; a provider's endpoint registration
  chooses its events.

## Alternatives Considered

1. **Unsigned webhooks for senders that cannot sign** - rejected: it is the
   forged-event attack. `token` covers senders that can only send a secret.
2. **Fire `kind = "webhook"` through the `http` adapter** - rejected for now:
   the adapter ignores `headers` and `body_template`, so the declaration would
   install and silently drop half of what it says, and it bypasses the egress
   gate WASM calls go through.
