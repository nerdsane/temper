# Feature map

Surface enumeration.

Served route trees (crates/temper-server/src): `/tdata` (OData), `/observe` (UI + health), `/api` (authorize, decisions, policies, audit, repl), `/_admin` (profiling), `/healthz`.
CLI verbs (temper-cli): Serve, Mcp, Verify, VerifyIoa, VerifyRemote, Init, Codegen, Install, Decide, MigrateTursoToPostgres.
Plus the DST suites (crates/temper-platform/tests).

| Feature | File | Drive when you changed |
|---|---|---|
| Serve + OData | serve-and-odata.md | server, routes, stores, platform bootstrap |
| Spec cascade | spec-cascade.md | any `.ioa.toml`, temper-spec, temper-verify |
| DST proof | dst-proof.md | temper-runtime, temper-jit, temper-server sim paths |
| MCP bridge + REPL | mcp-bridge.md | temper-mcp, temper-sandbox, SDK surface |
| Observe UI + decisions | observe-ui.md | temper-observe, temper-authz, approval flow |

## Not yet mapped

- `/api` governance routes (authorize, policies, audit) - the Cedar policy/audit surface; decisions is partially covered by observe-ui.md, the rest needs its own file
- `/_admin` profiling (cpu/wall) - ops-only; drive read-only

- Init/Codegen - scaffolding verbs; drive = run them in a temp dir and build the output
- Install - app install flow; needs a target app checkout
- Decide (CLI) - covered indirectly by observe-ui.md's decision flow
- MigrateTursoToPostgres - one-way ops migration; drive only against scratch data
