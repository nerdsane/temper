# ADR-0107: WASM Lazy Secret Authorization

- Status: Proposed
- Date: 2026-05-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0012: OAuth2 Enablement - Webhooks, Timers, and Secret Templates
  - ADR-0075: Tenant Secrets Key Management
  - ADR-0084: Authz Latency Phase Instrumentation
  - ADR-0106: WASM Integration Envelope Attribution
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-server/src/state/dispatch/wasm_secrets.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

ADR-0106 moved the remaining WASM integration envelope into Datadog child spans. The first live
TemperPaw proof after deployment found that `dispatch.wasm.phase.authz_secret_resolution` is now the
largest proven server-side phase outside guest work:

- Single controlled after trace: `authz_secret_resolution` about 72 ms for the provider-response
  applier.
- Five-session after batch: `authz_secret_resolution` count 20, average about 79 ms, p50 about
  76 ms, p95 about 88 ms.
- The phase appears on every WASM integration in a session, including modules whose config does not
  reference `{secret:...}` and whose successful path does not call `host_get_secret`.

The current dispatch path obtains the merged platform+tenant secret map, clones all plaintext
values, and evaluates Cedar `access_secret` for every key before constructing the host. That was a
reasonable defense-in-depth implementation when the secret set was small, but it turns every WASM
invocation into an O(number of visible secrets) authorization pass even when the module is
effectively secretless.

This is an overlooked host-bridge cost, not a fundamental Temper architecture limitation. Temper's
mission still requires generated specs, Cedar governance, tenant isolation, replayable audit, and
secret safety. The fix must make the common path fast without putting unauthorized secrets into
memory or allowing stale authorization decisions to drift.

## Decision

### Move Guest Secret Reads To Lazy Per-Key Authorization

`ProductionWasmHost` will support an optional secret resolver callback. When the resolver is
configured, guest `host_get_secret` calls will use it as the authoritative secret path. The eager
in-memory secret map remains available for host-internal bootstrap behavior, not as the guest secret
read path.

Temper Server will install an authorization-aware resolver that:

- Receives the requested secret key at `host_get_secret` time.
- Evaluates Cedar `access_secret` for exactly that key and the invocation's `WasmAuthzContext`.
- Reads the current tenant/platform secret value only after Cedar allows the key.
- Returns the same "secret not found" shape when the value is absent.
- Does not cache plaintext values across invocations.

**Why this approach**: Most hot TemperPaw session integrations do not need arbitrary secrets on the
successful path. Lazy per-key authorization changes the cost from "authorize every secret on every
WASM invocation" to "authorize the secret actually requested." Correctness remains tied to the live
Cedar engine and the live secrets vault, so policy reloads and secret updates take effect without a
new invalidation protocol.

### Keep Host Bootstrap Secrets Eager And Bounded

The host still needs a small set of server-side bootstrap values before the guest runs:

- `blob_endpoint`, used to detect and short-circuit configured blob transport URLs.
- `temper_api_url`, used to detect and short-circuit local file-value transport URLs.
- `temper_api_key`, used as the fallback internal API bearer token when no ambient
  `TEMPER_API_KEY` is configured.
- `ca_cert:*`, used to add tenant-provisioned CA roots to the outbound HTTP client.

Temper Server will build an eager bootstrap map by listing secret keys only, selecting the known
bootstrap keys/prefixes, authorizing those selected keys, and then fetching only allowed values.
Other secrets stay out of the host's eager map.

**Why this approach**: It preserves existing transport behavior and private-CA support while bounding
startup authorization by the small host-bootstrap surface instead of the entire tenant secret set.

### Preserve Defense In Depth

The outer `AuthorizedWasmHost` will continue to authorize `host_get_secret` before delegating. The
new resolver will also authorize before reading the vault, and `ProductionWasmHost` will prefer the
resolver over its eager bootstrap map for guest secret lookups. This means arbitrary secrets are not
eagerly materialized and guest secret reads remain authorization-gated even if a key is also needed
for host bootstrap.

**Why this approach**: ADR-0004 explicitly requires secret pre-filtering as defense in depth. The new
shape preserves the security property as "authorize before value materialization" rather than
"authorize all values before invocation." Unauthorized plaintext values are not eagerly cloned into
the host.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add tests that prove the eager path authorizes only bootstrap secrets
   and that lazy `get_secret` authorizes and reads only the requested key.
2. **Phase 1 (Temper core PR)** - Add the resolver callback to `ProductionWasmHost`, add the
   bounded bootstrap-secret helper in Temper Server, and wire the dispatch and direct-invocation
   host paths through the lazy resolver.
3. **Phase 2 (TemperPaw bump and deploy)** - Rebuild TemperPaw on the merged Temper revision and
   deploy to production.
4. **Phase 3 (Before/after proof)** - Re-run the same live Session proof and batch used for
   ADR-0106. The report must compare the pre-change `authz_secret_resolution` baseline to the
   post-change Datadog distribution and controlled trace.

## Readiness Gates

- Unit tests prove bootstrap selection does not authorize or load arbitrary non-bootstrap secrets.
- Unit tests prove lazy secret reads return allowed values, deny unauthorized values, and reflect
  updated vault values without stale cache.
- Existing `temper-wasm` and `temper-server` WASM dispatch tests pass.
- `cargo fmt --all -- --check` and targeted clippy pass.
- Live TemperPaw sessions complete correctly after deployment.
- Datadog after traces show lower `dispatch.wasm.phase.authz_secret_resolution` duration with no
  increase in `wasm.host.get_secret` errors or authorization-denial regressions.
- `docs/temper-latency-observability-report.html` records before and after timings, trace links,
  Datadog distributions, correctness checks, and any residual latency.

## Consequences

### Positive

- Removes O(number of secrets) Cedar work from the common WASM invocation path.
- Keeps Cedar, tenant isolation, and secret update correctness live at the moment of access.
- Reduces plaintext secret materialization in hot hosts.
- Produces a clean before/after target for the latency program.

### Negative

- Actual `host_get_secret` calls now pay per-key authorization at call time.
- `ProductionWasmHost` gains a resolver callback surface, making the host slightly more abstract.
- The host-bootstrap key list must remain intentionally small and reviewed when new bootstrap needs
  appear.

### Risks

- A module that relied on arbitrary eager secrets without calling `host_get_secret` would break. The
  current public host contract exposes secrets through `host_get_secret`, so this would indicate an
  undocumented coupling rather than a supported API.
- Missing a bootstrap key could disable a local transport optimization. The mitigation is a focused
  allowlist and tests for blob/file transport bootstrap keys.
- Double authorization on real `host_get_secret` calls adds overhead for secret-heavy modules. That
  is acceptable because secret-heavy modules are not the hot path being optimized; a later PR can
  introduce request-local memoization if Datadog proves it is necessary.

### DST Compliance

- This change touches `temper-server`, which is simulation-visible, and `temper-wasm`, which hosts
  production effects.
- No actor transition, scheduler, mailbox, persistence, spec, or replay decision changes.
- Secret vault access, Cedar authorization, HTTP client construction, and wall-clock trace timing are
  production host behavior, not deterministic simulation inputs.
- Existing `BTreeMap` ordering remains in the eager bootstrap map.

## Non-Goals

- This ADR does not bypass Cedar authorization for secrets.
- This ADR does not cache plaintext secrets across invocations.
- This ADR does not change secret template syntax or persistence.
- This ADR does not remove the outer `AuthorizedWasmHost`.
- This ADR does not claim a latency win until live before/after evidence is recorded.

## Alternatives Considered

1. **Cache the full authorized secret map per module** - Rejected for this PR because it creates
   policy and secret invalidation complexity, risks stale plaintext values, and still materializes
   many unused secrets.
2. **Remove secret pre-filtering entirely and rely only on `AuthorizedWasmHost`** - Rejected because
   ADR-0004 requires defense in depth if the outer decorator is bypassed or miswired.
3. **Hardcode a secretless fast path by module name** - Rejected because it would violate Temper's
   generated-app architecture and turn runtime behavior into a TemperPaw-specific special case.
4. **Require specs to declare every secret dependency first** - Attractive longer-term governance,
   but too large for the immediate measured latency improvement and not necessary to remove the
   current O(all secrets) cost.

## Rollback Policy

Rollback is code-only: restore eager authorized secret map construction and remove the resolver
callback wiring. No data migration, spec change, or secret-store migration is required. If live
traces show regressions, keep ADR-0106 envelope spans enabled so the rollback and follow-up can be
measured with the same before/after protocol.
