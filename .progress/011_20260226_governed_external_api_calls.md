# 011: Governed External API Calls Through MCP

**ADR:** docs/adrs/0005-governed-external-api-calls-through-mcp.md
**Date:** 2026-02-26
**Status:** COMPLETE

## Phases

### Phase 1: Integration Config Fields (Parts 1a-1c)
- [x] Add `config: BTreeMap<String, String>` to Integration struct (types.rs)
- [x] Update TOML parser to store unknown keys in config (parser.rs)
- [x] Add `integration_config` to WasmInvocationContext (wasm types.rs)
- [x] Add test for config parsing (`test_integration_config_captures_unknown_keys`)

### Phase 2: Fix trigger_params + Pass Config (Parts 1d + 4)
- [x] Thread action params through dispatch_wasm_integrations
- [x] Thread action params through dispatch_wasm_integrations_blocking
- [x] Pass integration.config to WasmInvocationContext
- [x] Pass actual trigger_params (not null)

### Phase 3: upload_wasm MCP Method (Part 3)
- [x] Add `upload_wasm` method to tools.rs
- [x] Add `temper_request_bytes` helper to runtime context
- [x] Update protocol.rs tool descriptions (added upload_wasm + http_fetch mention)
- [x] Add upload_wasm to unknown-method error message

### Phase 4: Generic http_fetch WASM Module (Part 2)
- [x] Create http-fetch WASM module (examples/wasm-modules/http-fetch/)
- [x] Build and copy to crates/temper-wasm/modules/http_fetch.wasm
- [x] Add `builtins` support to WasmModuleRegistry
- [x] Pre-register http_fetch at startup via `register_builtin_wasm_modules()`

### Phase 5: Tests + Verification
- [x] cargo test -p temper-spec (42/42 pass)
- [x] cargo test -p temper-wasm (4/4 pass)
- [x] cargo test -p temper-server (121/121 pass)
- [x] cargo test -p temper-mcp (20/20 pass)
- [x] cargo check --workspace (clean)

## Files Changed

| File | Change |
|------|--------|
| `crates/temper-spec/src/automaton/types.rs` | Added `config: BTreeMap<String, String>` to Integration |
| `crates/temper-spec/src/automaton/parser.rs` | Store unknown keys in config, init config in constructor, added test |
| `crates/temper-wasm/src/types.rs` | Added `integration_config` to WasmInvocationContext |
| `crates/temper-wasm/tests/e2e_invoke.rs` | Added `integration_config` to test context |
| `crates/temper-server/src/state/dispatch.rs` | Pass config + actual trigger_params, added action_params param |
| `crates/temper-server/src/state/dispatch_blocking.rs` | Same as dispatch.rs |
| `crates/temper-server/src/wasm_registry.rs` | Added `builtins` field + `register_builtin()` + fallback in `get_hash()` |
| `crates/temper-server/src/state/mod.rs` | Pre-register http_fetch WASM module at startup |
| `crates/temper-mcp/src/tools.rs` | Added `upload_wasm` method + `temper_request_bytes` helper |
| `crates/temper-mcp/src/protocol.rs` | Updated execute tool description with upload_wasm + http_fetch |
| `examples/wasm-modules/http-fetch/` | New generic HTTP fetch WASM module |
| `crates/temper-wasm/modules/http_fetch.wasm` | Compiled WASM binary (48KB) |
| `Cargo.toml` | Added http-fetch to workspace exclude |
