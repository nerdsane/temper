# Feature map

Surface enumeration.

Served route trees (`crates/temper-server/src`): `/tdata` (OData reads, bound actions, `$metadata`, `$hints`, `$events`), `/observe` (UI, entity history, replay-parity, specs, health), `/api` (authorize, decisions, policies, audit, specs load/validate), `/webhooks/{tenant}/{*path}` (inbound signed webhooks), `/_admin` (profiling), `/healthz`.
CLI verbs (`temper-cli`): `serve`, `mcp`, `verify`, `verify-ioa`, `init`, `codegen`, `install`, `decide`, `migrate-turso-to-postgres`.
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
- `install` - app install flow; needs a target app checkout (temperpaw's genesis-install covers the app side).
- `migrate-turso-to-postgres` - one-way ops migration; drive only against scratch data.
- Composite cross-entity verification (ADR-0150) runs inside `temper verify` for multi-entity dirs; it is documented in spec-cascade.md rather than its own file.
- Trajectory / OTS audit readback - the write path is `POST /api/audit`; there is no `/api/audit` reader, so read through the trajectory/observe endpoints when a change touches it.
