# ADR-0009: WASM Developer Experience

- Status: Accepted
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0002: WASM Sandboxed Integration Runtime
  - ADR-0007: Governed External API Calls Through the MCP REPL
  - `.vision/` (code is derived from specs and is regenerable)
  - `crates/temper-wasm` (WASM engine and host functions)
  - `crates/temper-mcp` (MCP tool dispatch)

## Context

Agents using Temper declare `[[integration]]` sections in IOA specs for external API calls. The builtin `http_fetch` module handles simple HTTP declaratively. But custom integration logic (response transforms, conditional flows, multi-step orchestration) requires:

1. Writing Rust targeting `wasm32-unknown-unknown` with raw pointer/length ABI
2. Having the Rust toolchain with the wasm32 target
3. Manual `cargo build --target wasm32-unknown-unknown --release`
4. Manual `upload_wasm(tenant, name, path)` via MCP

This breaks the vision: agents escape the governed sandbox, need Rust knowledge, and work with unsafe low-level ABI. The spec says "code is derived from specs and is regenerable" — but custom WASM modules are hand-written artifacts outside the governance boundary.

## Decision

Three sub-decisions:

### Sub-Decision 1: `temper-wasm-sdk` Crate

Lightweight SDK hiding the raw pointer/length ABI. Agents write:

```rust
use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let resp = ctx.http_get(&ctx.config["url"])?;
        let data: Value = serde_json::from_str(&resp.body)?;
        Ok(json!({ "temperature": data["current"]["temperature_2m"] }))
    }
}
```

Instead of 375 lines of raw pointer manipulation.

SDK provides:
- `temper_module!` macro — generates `extern "C" fn run`, memory management, JSON ser/de
- `Context` struct — typed wrapper with config, trigger_params, entity_state access
- `ctx.http_get(url)` / `ctx.http_post(url, body)` / `ctx.http_call(method, url, headers, body)`
- `ctx.get_secret(key)` — typed wrapper for `host_get_secret`
- `ctx.log(level, msg)` — typed wrapper for `host_log`
- `Result<Value>` return — auto-generates success/failure callback JSON
- Re-exports `serde_json::{json, Value}`

Uses `macro_rules!` (not a proc macro) to avoid a separate `-macros` crate.

**Why this approach**: The SDK is a thin compile-time wrapper. Zero runtime overhead. The macro generates the same `extern "C" fn run` signature the engine expects. Agents get type safety and ergonomics without any changes to the WASM engine.

### Sub-Decision 2: `compile_wasm` MCP Tool

New MCP method accepting Rust source code and handling compilation:

```python
result = await temper.compile_wasm("my-app", "weather_fetcher", rust_source)
```

Implementation flow:
1. Agent submits Rust source via sandbox
2. MCP tool creates temp dir with scaffolded Cargo.toml pointing to temper-wasm-sdk
3. Writes agent source to `src/lib.rs`
4. Runs `cargo build --target wasm32-unknown-unknown --release` with 120s timeout
5. On success: reads .wasm binary, uploads to server, returns hash + size
6. On failure: returns compiler stderr for agent self-correction
7. Cleans up temp dir

Key details:
- Shared CARGO_HOME for dependency caching
- Prerequisite check for wasm32-unknown-unknown target
- Agent source compiled in isolated temp dir; output is sandboxed WASM

**Why this approach**: Keeps compilation local (MCP process), returns compiler errors for iterative fixing. No new infrastructure needed.

### Sub-Decision 3: Expanded Builtin Modules (Deferred)

Not implemented yet, documented for future:
- `http_chain` — sequential HTTP calls with data threading
- `json_transform` — JMESPath/JSONPath response transforms
- `conditional` — branch on response status/content

**Why defer**: SDK + compile_wasm covers custom logic needs immediately. Builtins are optimizations for common patterns discovered through usage.

## Rollout Plan

1. **Phase 0 (This PR)**: temper-wasm-sdk crate + compile_wasm MCP tool + http-fetch rewrite
2. **Phase 1 (Follow-up)**: Integration tests for full cycle (spec -> compile -> deploy -> invoke)
3. **Phase 2 (Future)**: Expanded builtin modules based on usage patterns

## Consequences

### Positive
- Agents author WASM modules in-sandbox without leaving governance boundary
- 375 lines of raw ABI code reduced to ~15 lines with SDK
- Compiler errors returned to agent for self-correction (no human intervention)
- SDK is zero-overhead (compile-time wrapper only)

### Negative
- Compilation adds latency (~30-60s first build, faster incremental)
- Requires wasm32-unknown-unknown target installed on MCP host
- SDK must be kept in sync with engine host function signatures

### Risks
- Cargo builds in temp dirs may fail on unusual host configurations
- Large dependency trees could slow compilation
- Mitigation: shared CARGO_HOME and target dir caching

## Non-Goals

- JavaScript/AssemblyScript compilation support
- Server-side compilation (always local to MCP process)
- WASM Component Model migration (module interface stays simple)
- Expanded builtin modules beyond http_fetch (deferred)

## Alternatives Considered

1. **Proc macro crate** — Would give `#[temper_module]` attribute syntax but requires a separate `temper-wasm-sdk-macros` crate. Rejected: `macro_rules!` is simpler and sufficient.
2. **AssemblyScript/JS compilation** — Lower barrier than Rust but adds a compiler dependency and loses type safety. Rejected: Rust is the project language and WASM target is mature.
3. **Code generation from spec** — Auto-generate WASM modules from declarative config. Rejected for now: too rigid for custom logic. The SDK serves the escape hatch use case.
4. **Server-side compilation service** — Compilation as a hosted service. Rejected: adds infrastructure complexity for a local-only use case.
