# 012: Agent Governance UX — Default-Deny with Human Approval

## Status: In Progress

## Phases

### Phase 0: ADR-0008 [DONE]
- [x] Write ADR-0008

### Phase 1: Fix Immediate Bugs [DONE]
- [x] 1a. WASM engine result reading fix (engine.rs) — host_set_result checked first
- [x] 1b. Fix set_policy content type (tools.rs) — removed entirely (governance write)
- [x] 1c. Default Turso storage (tools.rs) — start_server passes --storage turso

### Phase 2: Remove Governance Write Methods [DONE]
- [x] 2a. Remove approve_decision, deny_decision, set_policy from dispatch (tools.rs)
- [x] 2b. Update tool description (protocol.rs)
- [x] 2c. Enhance authz denied error (sandbox.rs) — includes poll_decision guidance

### Phase 3: Observe UI Auto-Start [DONE]
- [x] 3a. --observe flag for temper serve (serve/mod.rs)
- [x] 3b. MCP start_server starts Observe (tools.rs) — passes --observe

### Phase 4: Claude Code Hook + temper decide CLI [DONE]
- [x] 4a. PostToolUse hook (postToolUse-temper-decide.sh)
- [x] 4b. temper decide CLI subcommand (decide/mod.rs)

### Phase 5: http_fetch Hardening [DONE]
- [x] Harden http_fetch module — includes status_code in response
- [x] Rebuild WASM binary

### Phase 6: Verification [DONE]
- [x] Build passes (temper-wasm, temper-mcp, temper-cli all compile)
- [x] Tests pass (temper-wasm: 20, temper-mcp: 20, temper-cli: 29, temper-server: 2)
