# WASM Persistence — Survive Server Restarts

## Status: COMPLETE

## Steps
- [x] Step 1: Add wasm_invocation_logs schema (Postgres + Turso)
- [x] Step 2: Add migrations for new table (+ fixed missing wasm_modules migration in Postgres)
- [x] Step 3: Add Turso store methods (persist_wasm_invocation, load_recent_wasm_invocations, load_wasm_modules_all_tenants)
- [x] Step 4: Add ServerState persistence methods (persist_wasm_invocation, load_wasm_modules, load_recent_wasm_invocations)
- [x] Step 5: Wire dispatch.rs to persist invocations (fire-and-forget after each in-memory log push)
- [x] Step 6: Wire startup recovery in serve/mod.rs (load_wasm_modules + load_recent_wasm_invocations)
- [x] Step 7: Update observe/wasm.rs (kept in-memory fast path, limit raised to 10k)
- [x] Step 8: Run tests — 697 workspace tests pass, 0 failures

## Files Changed
| File | Action |
|------|--------|
| `crates/temper-store-postgres/src/schema.rs` | Added `CREATE_WASM_INVOCATION_LOGS_TABLE` + 3 indexes + tests |
| `crates/temper-store-turso/src/schema.rs` | Same for Turso (SQLite syntax) |
| `crates/temper-store-postgres/src/migration.rs` | Added wasm_modules + invocation_logs migrations |
| `crates/temper-store-turso/src/store.rs` | Added persist/load methods + TursoWasmInvocationRow |
| `crates/temper-store-turso/src/lib.rs` | Exported new types (TursoWasmInvocationInsert, TursoWasmInvocationRow) |
| `crates/temper-server/src/state/persistence.rs` | Added persist_wasm_invocation, load_wasm_modules, load_recent_wasm_invocations |
| `crates/temper-server/src/state/dispatch.rs` | Fire-and-forget persist after each invocation log push |
| `crates/temper-server/src/observe/wasm.rs` | Raised limit to 10k, kept in-memory fast path |
| `crates/temper-cli/src/serve/mod.rs` | Startup recovery: load_wasm_modules + load_recent_wasm_invocations |

## Bug Fix
Fixed pre-existing bug: `wasm_modules` table was defined in Postgres schema.rs but missing from
migration.rs. Now included in the migration runner.
