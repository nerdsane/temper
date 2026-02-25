# Agent Backend Capabilities

**Created**: 2026-02-24
**Status**: Complete

## Phases

- [x] Phase 0: ADR — `docs/adrs/0003-agent-backend-capabilities.md`
- [x] Phase 1: Agent Identity / Sessions — X-Agent-Id, X-Session-Id threaded through dispatch
- [x] Phase 2: Credentials Vault — AES-256-GCM per-tenant secrets, injected into WASM host
- [x] Phase 3: Idempotency — Per-actor LRU cache with TTL eviction
- [x] Phase 4: Blocking Integrations — ?await_integration=true for inline WASM execution
- [x] Verification — `cargo test --workspace` all passing

## Files Changed

### New Files
- `docs/adrs/0003-agent-backend-capabilities.md` — ADR
- `crates/temper-server/src/secrets_vault.rs` — Encrypted vault with AES-256-GCM
- `crates/temper-server/src/idempotency.rs` — Idempotency LRU cache
- `crates/temper-server/src/state/dispatch_blocking.rs` — Blocking WASM integration dispatch

### Modified Files
- `Cargo.toml` + `Cargo.lock` — aes-gcm workspace dep
- `crates/temper-server/Cargo.toml` — aes-gcm dep
- `crates/temper-server/src/dispatch.rs` — AgentContext, idempotency, await_integration
- `crates/temper-server/src/state/dispatch.rs` — Agent threading, secrets injection, blocking branch
- `crates/temper-server/src/state/mod.rs` — New fields: idempotency_cache, secrets_vault
- `crates/temper-server/src/state/trajectory.rs` — agent_id, session_id fields
- `crates/temper-server/src/state/persistence.rs` — Trajectory + secrets persistence
- `crates/temper-server/src/events.rs` — agent_id, session_id on EntityStateChange
- `crates/temper-server/src/router.rs` — CORS headers for new headers
- `crates/temper-server/src/api.rs` — Secrets management routes
- `crates/temper-server/src/lib.rs` — Module registration
- `crates/temper-wasm/src/types.rs` — agent_id, session_id on WasmInvocationContext
- `crates/temper-store-postgres/src/schema.rs` — Trajectories + secrets table schemas
- Various test files updated for new struct fields

## Instance Log

| Phase | Status | Notes |
|-------|--------|-------|
| 0 | Complete | ADR created |
| 1 | Complete | Agent identity threaded through full dispatch chain |
| 2 | Complete | Vault + API + WASM injection |
| 3 | Complete | Idempotency cache with budget + TTL |
| 4 | Complete | Blocking dispatch with Box::pin for recursive async |
| Verify | Complete | cargo test --workspace all passing |
