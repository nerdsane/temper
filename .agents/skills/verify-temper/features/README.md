# Feature map

Surface enumeration (from `temper-cli` subcommands + served routes): Serve (OData API + Observe UI), Mcp (stdio bridge + REPL), Verify/VerifyIoa/VerifyRemote (cascade), DST suites, Init/Codegen (scaffolding), Install, Decide (approval CLI), MigrateTursoToPostgres (ops migration).

| Feature | File | Drive when you changed |
|---|---|---|
| Serve + OData | serve-and-odata.md | server, routes, stores, platform bootstrap |
| Spec cascade | spec-cascade.md | any `.ioa.toml`, temper-spec, temper-verify |
| DST proof | dst-proof.md | temper-runtime, temper-jit, temper-server sim paths |
| MCP bridge + REPL | mcp-bridge.md | temper-mcp, temper-sandbox, SDK surface |
| Observe UI + decisions | observe-ui.md | temper-observe, temper-authz, approval flow |

## Not yet mapped

- Init/Codegen - scaffolding verbs; drive = run them in a temp dir and build the output
- Install - app install flow; needs a target app checkout
- Decide (CLI) - covered indirectly by observe-ui.md's decision flow
- MigrateTursoToPostgres - one-way ops migration; drive only against scratch data
