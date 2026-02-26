# ADR-0007: Governed External API Calls Through the MCP REPL

- Status: Accepted
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0002: WASM Sandboxed Integration Runtime
  - `.vision/AGENT_OS.md` (external access is governed)
  - `crates/temper-spec/src/automaton/types.rs` (Integration struct)
  - `crates/temper-wasm/src/types.rs` (WasmInvocationContext)
  - `crates/temper-mcp/src/tools.rs` (MCP method dispatch)

## Context

The Agent OS vision states: "When an agent needs to call an outside system, it does so through an integration declared in the IOA spec." ADR-0002 established the WASM sandbox for integration execution. The integration engine works end-to-end: specs declare `[[integration]]` sections, WASM modules fire on action effects, callbacks re-enter the entity state machine.

However, an agent operating through the MCP REPL cannot complete the external API call loop today because of three gaps:

1. **No integration config in specs.** `[[integration]]` sections carry trigger/module/callbacks but cannot carry configuration like URL, method, or headers. The WASM module has no way to know which API to call unless it's hardcoded.

2. **No generic HTTP module.** Every integration requires a purpose-built compiled WASM binary. Agents cannot compile Rust to WASM through the REPL. This forces every external API call to require a pre-built binary, breaking the "agent generates and submits everything" workflow.

3. **No WASM upload in MCP.** The server has `POST /api/wasm/modules/{name}` but the MCP `temper` object doesn't expose it. For custom integrations that need logic beyond generic HTTP (response parsing, multi-step flows), the agent has no way to upload a module through the REPL.

Additionally, `trigger_params` in `WasmInvocationContext` is hardcoded to `null` — the WASM module cannot access the action parameters that triggered it.

## Decision

### Sub-Decision 1: Integration config fields via `#[serde(flatten)]`

Extend the `Integration` struct with a catch-all config map:

```toml
[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://api.open-meteo.com/v1/forecast"
method = "GET"
```

Any key not in the known set (`name`, `trigger`, `type`, `module`, `on_success`, `on_failure`) is stored in `config: BTreeMap<String, String>` and passed to the WASM module via a new `integration_config` field on `WasmInvocationContext`.

Why flatten instead of a nested `[integration.config]` table: keeps the spec flat and readable for agents generating TOML. Common keys (`url`, `method`, `headers`) sit at the same level as `trigger` and `module`. No nested structure to get wrong.

### Sub-Decision 2: Generic `http_fetch` WASM module ships with the server

A pre-built WASM module named `http_fetch` is embedded in the server binary and registered in every tenant's `WasmModuleRegistry` at startup. The module:

1. Reads `url` and `method` from `integration_config`
2. Reads `headers` from `integration_config` (JSON-encoded string, optional)
3. Appends `trigger_params` as query parameters (GET) or JSON body (POST)
4. Calls `host_http_call`
5. Returns the response body as callback params via `host_set_result`

This means an agent can call any HTTP API by generating a spec alone — no WASM compilation, no binary upload, no toolchain. The agent declares the URL in the spec, the runtime handles execution within governance boundaries.

Why a pre-built module instead of a declarative HTTP integration type: the WASM sandbox is the security boundary. A new `type = "http"` that bypasses the sandbox would create a second execution path with different security properties. Keeping everything as WASM means one sandbox, one authorization gate, one audit mechanism.

### Sub-Decision 3: `upload_wasm` MCP method for custom modules

For integrations that need logic beyond generic HTTP fetch (response parsing, multi-step API flows, custom error handling), add an `upload_wasm(tenant, module_name, wasm_path)` method to the MCP `temper` object. The method reads the WASM binary from the filesystem path and POSTs it to `/api/wasm/modules/{module_name}`.

The agent provides a file path, not raw bytes, because the Monty sandbox cannot handle binary data. File I/O happens in the MCP host process, outside the sandbox.

### Sub-Decision 4: Fix trigger_params passthrough

Thread actual action parameters through `dispatch_wasm_integrations` into `WasmInvocationContext.trigger_params`. This allows the WASM module to access dynamic values provided at action invocation time (e.g., latitude/longitude for a weather query, customer ID for a payment charge).

## Consequences

### Positive

- **Agent self-sufficiency.** An agent can call any HTTP API by generating a spec with `[[integration]]` config — no toolchain, no pre-built binaries, no external setup.
- **Single security model.** All external calls flow through the WASM sandbox, authorized by Cedar, recorded in trajectory logs. No bypass path.
- **Backward compatible.** Existing specs without config fields continue to work unchanged. The `config` map defaults to empty.
- **Extensible.** Custom WASM modules (via `upload_wasm`) handle complex integrations. The generic module covers the common case.

### Negative

- **Embedded WASM binary increases server binary size** (~50-100KB for http_fetch, negligible).
- **Config-in-spec is stringly typed.** `BTreeMap<String, String>` is flexible but not validated at parse time. Invalid URLs or methods fail at runtime, not at spec submission. Future: schema validation per module.
- **`upload_wasm` requires file path.** The agent must have the WASM binary on disk. This works for local development but requires a different mechanism for remote/cloud agents.

### Risks

- **Generic module scope creep.** The `http_fetch` module should stay simple (URL + method + headers). Complex response transformation should use custom WASM modules.
- **Config key collisions.** If `[[integration]]` ever needs a new standard field that clashes with a config key agents already use, migration is needed. Mitigated by reserving common field names.

## Alternatives Considered

1. **New `type = "http"` integration** — Server makes HTTP calls directly without WASM. Simpler but creates a second execution path outside the WASM sandbox, undermining the single-security-model principle from ADR-0002.

2. **Server-side WASM compilation** — Agent submits Rust source, server compiles to WASM. Complex infrastructure (needs Rust toolchain in server image), significant security surface, slow compilation. Deferred to future work.

3. **Action params only (no config in spec)** — Agent passes URL at action invocation time rather than declaring it in the spec. Works but means the URL isn't part of the verified spec — the same spec could call different APIs depending on what the agent passes. Config-in-spec makes the integration target inspectable and auditable.

4. **Nested `[integration.config]` table** — More structured but harder for agents to generate correctly in TOML. Flat keys are simpler for LLM code generation.
