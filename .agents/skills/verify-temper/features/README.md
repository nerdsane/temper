# Feature map

Surface enumeration.

Served route trees (`crates/temper-server/src`): `/tdata` (OData reads, bound actions, `$metadata`, `$hints`, `$events`), `/observe` (UI, entity history, replay-parity, specs, health), `/api` (authorize, decisions, policies, audit, specs load/validate), `/webhooks/{tenant}/{*path}` (inbound signed webhooks), `/_admin` (profiling). The unauthenticated `/healthz` (liveness) and `/version` (deploy-identity, returns `{commit}`) routes are registered one layer up in `crates/temper-platform/src/router.rs` (`build_platform_router`), not in temper-server; both are reserved built-ins that take precedence over any tenant HttpEndpoint at those paths.
CLI verbs (`temper-cli`): `serve`, `mcp`, `verify`, `verify-ioa`, `verify-remote`, `audit`, `init`, `codegen`, `install`, `decide`, `migrate-turso-to-postgres`.
Plus the DST suites (`crates/temper-server/tests/dst_*`, `crates/temper-platform/tests`).

| Feature | File | Drive when you changed |
|---|---|---|
| Serve + OData | serve-and-odata.md | server, routes, stores, platform bootstrap |
| Entity lifecycle | entity-lifecycle.md | create/dispatch/readback, action params, state application |
| Cedar authz + decisions | cedar-authz.md | temper-authz, /api policies, the approval flow |
| Query surface | query-surface.md | temper-odata, $filter/$expand/paging, DoS caps |
| Event-sourcing readback | event-sourcing-readback.md | temper-runtime persistence, EntityActor, snapshots, replay |
| Spec cascade | spec-cascade.md | any `.ioa.toml`, temper-spec, temper-verify |
| Spec hot-swap | spec-hot-swap.md | registry, temper-jit swap, live spec versions |
| WASM integration | wasm-integration.md | temper-wasm, `[[action.triggers]]`, module dispatch |
| Integrations + webhooks | integrations-and-webhooks.md | outbound integrations, inbound signed webhooks, HTTP endpoints |
| Blobs + TemperFS | blobs-and-temperfs.md | field overflow, blob store, media/file streams |
| DST proof | dst-proof.md | temper-runtime, temper-server sim paths, determinism |
| MCP bridge + REPL | mcp-bridge.md | temper-mcp, temper-sandbox, SDK surface |
| Observe UI | observe-ui.md | temper-observe, the browser surface |

## Not yet mapped

- `/_admin` profiling (cpu/wall) - ops-only; drive read-only.
- `init` / `codegen` - scaffolding verbs; drive = run them in a temp dir and build the output.
- `verify-remote` - **broken against a local SKILL Launch serve.** The CLI POSTs `/api/specs/validate-ioa` with `X-Temper-Principal-Kind: admin` and no bearer (`crates/temper-cli/src/verify_remote.rs`). That header is stripped (`authz/edge.rs`); the route is not public → **401**. Even with a bearer, Cedar `run_verification` is not on the operator seed (only `manage_policies` on PolicySet) → **403**. Do not drive it. Offline `temper verify` / `verify-ioa` in spec-cascade.md is the working cascade.
- `audit` - re-checks spec invariants against live entities (`crates/temper-cli/src/audit.rs`). The check logic is `temper_jit::audit`, covered by unit tests including one against `test-fixtures/specs/order.ioa.toml`. The HTTP path **has been run against a live server** (local `serve` on :4455, 72 entities across two app entity types): it GETs `/tdata` for the service document, then `/tdata/{set}?$top=500` with a bearer from `$TEMPER_TOKEN` and `X-Tenant-Id`; both headers are required together. Note that the service document lists Temper's **own internal sets** (`Agents`, `Policies`, `AgentCredentials`, ...), and an app-scoped token is refused on all of them -- the verb skips a refused set, names the skipped set list once, and only fails if *every* set was refused. A 401/403 is reported differently depending on whether a token and tenant were actually sent, so a policy refusal is not misreported as missing configuration. It does not page, and warns on stderr when a set fills the page limit.
- `install` - app install flow; needs a target app checkout (temperpaw's genesis-install covers the app side).
- `migrate-turso-to-postgres` - one-way ops migration; drive only against scratch data.
- Composite cross-entity verification (ADR-0150) runs inside `temper verify` for multi-entity dirs; it is documented in spec-cascade.md rather than its own file.
- Trajectory / OTS audit readback - the write path is `POST /api/audit`; there is no `/api/audit` reader, so read through the trajectory/observe endpoints when a change touches it.
