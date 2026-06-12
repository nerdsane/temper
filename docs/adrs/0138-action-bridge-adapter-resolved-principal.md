# ADR-0138: HttpEndpoint Action Bridge Accepts Adapter-Resolved Principals

## Status

Accepted

## Context

Protocol adapter WASM integrations (git smart-HTTP, GitHub REST shims) sit in
front of the HttpEndpoint action bridge: the adapter parses the wire protocol
and returns typed `action_params`; the bridge dispatches the spec-configured
entity action. Authentication for these protocols does not arrive as
`X-Temper-*` headers — a `git push` carries a Basic/Bearer credential that
maps to a domain token entity (e.g. Genesis `GitToken`), which only the
adapter knows how to resolve.

Today `dispatch_action_bridge_result` builds the dispatch `SecurityContext`
from the inbound request headers and falls back to `SecurityContext::system()`
when the endpoint is registered without auth. The consequence downstream: a
`git push` into Genesis dispatches `Repository.IngestPack` as **system**, so
Cedar never evaluates the real caller, scope gates (e.g. `force` on
`Ref.ForceUpdate`) are unreachable, and the audit trail attributes pushes to
the system principal.

The adapter runs server-side as part of the trusted handler chain — it is the
component that actually authenticated the caller. The kernel just has no way
to receive its conclusion.

## Decision

The action bridge honors an adapter-resolved principal. An adapter may return,
alongside `action_params`:

```json
"bridge_principal": { "kind": "customer", "id": "user-1", "scopes": ["repo:push"] }
```

When present with non-empty `kind` and `id`, the bridge builds the dispatch
`SecurityContext` from these fields (same parsing path as the
`X-Temper-Principal-*` headers — the `system` kind is mapped to Customer, so
privilege cannot be smuggled) and uses it in preference to both the
inbound-header context and the system fallback. Malformed or empty values are
ignored and the existing behavior applies.

`bridge_principal` is honored only when the adapter also returned an explicit
`action_params` key (the structured result shape). Legacy passthrough
adapters whose whole result becomes the action params can leak request-derived
content into their result; they must never be able to hand the caller an
identity. The fallback path also strips the `bridge_*` control keys before
dispatching, so they never appear as action parameters.

An adapter may also short-circuit the request before any dispatch by
returning:

```json
"bridge_response": { "status": 401, "headers": { "WWW-Authenticate": "Basic realm=\"Genesis\"" }, "body": "authentication required" }
```

The bridge returns that response verbatim and skips action dispatch. This is
how a protocol adapter refuses an unauthenticated or malformed request with
protocol-correct semantics (the smart-HTTP 401 challenge) on a route whose
responses are otherwise bridge-formatted.

Presence of `bridge_response` always ends the request: a malformed value
(non-numeric status, invalid header name/value) fails closed as 502 rather
than falling through to dispatch — an adapter's refusal must never decay into
an action executed under the system fallback. The short-circuit applies only
to successful adapter results; an adapter returning `success=false` gets the
bridge-formatted error response regardless of any `bridge_response` value.

Both ADR-0138 control keys require the structured result shape: an explicit
`action_params` key must accompany `bridge_principal` (else it is ignored)
and `bridge_response` (else 502). A legacy passthrough adapter that echoes
client JSON as top-level params can therefore never hand a caller an identity
or response control on the kernel origin. Refusal-only adapters return
`"action_params": {}` alongside `bridge_response`.

This is a generic primitive: any protocol adapter with its own credential
scheme (git wire, REST compatibility shims, webhook signature verification)
can resolve a domain credential to a principal and have Cedar evaluate the
dispatched action as that principal.

## Consequences

- Genesis can enforce GitToken auth on push: the receive-pack adapter rejects
  anonymous callers at the wire level and returns the resolved principal, so
  `IngestPack` sub-writes (ref CAS, force-update gates) are Cedar-evaluated
  against the caller's scopes.
- The trust boundary is unchanged: adapters are operator-installed WASM
  already trusted to produce `action_params`; trusting their principal
  resolution adds no new trust domain.
- Endpoints whose adapters return no `bridge_principal` behave exactly as
  before.
- This changes which principal Cedar evaluates for bridge-dispatched actions
  — an authorization-semantics change, deliberately. It is DST-neutral: no
  state-machine, scheduling, or persistence changes, and the context is built
  from the adapter result with no new I/O.
