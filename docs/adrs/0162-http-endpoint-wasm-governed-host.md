# ADR-0162: HttpEndpoint WASM runs under the governed authorization host

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/router.rs` (HttpEndpoint dispatch)
  - `crates/temper-wasm/src/authorized_host.rs` (`AuthorizedWasmHost`)
  - `crates/temper-server/src/state/dispatch/wasm.rs` (governed entity-action path)
  - ARN-208 (security finding)

> This is Fable's competing entry for ARN-208; compared head-to-head by the arena judge.

## Context

WASM host calls are governed by `AuthorizedWasmHost`, a decorator that consults a
Cedar `WasmAuthzGate` before delegating to the inner host. It gates outbound HTTP
(`http_call`, `http_call_binary`, `connect_call`, `http_stream_begin_outbound`) via
`authorize_http_call`, and — critically — **secret access** (`get_secret`) via
`authorize_secret_access`. `ServerState::wasm_authz_gate()` always returns a
`CedarWasmAuthzGate` with **default-deny** semantics: a host call is denied unless a
permit policy matches.

The **entity-action** WASM dispatch path builds this governed host
(`wasm.rs`: `Arc::new(AuthorizedWasmHost::new(inner, gate, authz_ctx))`).

The **HttpEndpoint** dispatch path did not (`router.rs`):

```rust
let secrets = state.secrets_vault…get_tenant_secrets(tenant);   // ALL tenant secrets
let host = Arc::new(
    ProductionWasmHost::with_shared_streams(secrets, streams)    // raw, ungoverned
        .with_invocation_context(ctx),
);
engine.invoke_with_blobs(&hash, &ctx, host, &limits, …)          // no AuthorizedWasmHost
```

The raw `ProductionWasmHost` is handed straight to the engine. Its `get_secret`
returns the plaintext value with no authorization check, so **any WASM module bound
to an HttpEndpoint can read every tenant secret** — bypassing the Cedar gate that
governs the same operation everywhere else. Outbound HTTP from HttpEndpoint WASM is
likewise ungoverned.

## Decision

HttpEndpoint WASM runs under the **same governed host** as entity-action WASM.
`ServerState::build_http_endpoint_wasm_host` wraps the `ProductionWasmHost` (still the
inner host that owns the shared inbound/outbound streams) in `AuthorizedWasmHost`,
using `self.wasm_authz_gate()` and a `WasmAuthzContext` derived from the invocation
(`entity_type = "HttpEndpoint"`, `trigger_action = "HandleHttp"`, module, tenant).

Consequently, for HttpEndpoint WASM:
- `get_secret` is default-deny — a secret is returned only when a Cedar permit policy
  grants that module/tenant access, exactly as for entity-action WASM.
- Outbound HTTP is default-deny under the same policy surface.
- Inbound/outbound **body streaming** (`http_stream_read`/`_write`/`_close`/
  `response_head`) is delegated ungated, so the endpoint's own request/response
  handling is unchanged.

## Consequences

### Positive
- The plaintext-secret bypass is closed: HttpEndpoint WASM can no longer read tenant
  secrets outside Cedar governance. The authorization boundary is now uniform across
  every WASM entrypoint.

### Capability / migration
- This is the same contract entity-action WASM already lives under. An HttpEndpoint
  that legitimately needs a secret (or an outbound domain) declares a Cedar permit
  policy — the platform's standard mechanism. An endpoint that "worked" only because
  it read secrets ungoverned was exercising the vulnerability; it now requires an
  explicit permit. No governed capability is removed; an ungoverned one is brought
  under the existing policy surface. Endpoints that only read their request body and
  write their response body are unaffected.

### DST Compliance
- `temper-server` is simulation-visible. The change adds host wrapping (no wall
  clock, no threads, no `HashMap`, no ambient I/O in the new logic) and reuses the
  existing gate/context types. A pre-existing `tokio::spawn` touched incidentally is
  annotated `// determinism-ok` inline.

## Non-Goals / Follow-ups
- **Least-privilege secret snapshot.** The inner host is still seeded with the full
  tenant secret map (now gated at read time). Seeding only policy-permitted keys is a
  follow-up, independent of closing the gate bypass.

## Alternatives Considered
1. **Gate only `get_secret`, leave outbound HTTP ungoverned.** Rejected: partial
   governance is a band-aid; wrapping in `AuthorizedWasmHost` brings the whole
   entrypoint under the one policy surface, consistent with entity-action WASM.
2. **Pass no secrets to HttpEndpoint WASM at all.** Rejected: removes a legitimate
   capability (endpoints that are policy-permitted a secret); default-deny via Cedar
   is the correct, capability-preserving posture.
