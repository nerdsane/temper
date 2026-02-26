# ADR-0008: Agent Governance UX — Default-Deny with Human Approval

- Status: Accepted
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0007: Governed external API calls through MCP
  - ADR-0005: Agent policy and audit layer
  - `crates/temper-mcp/src/tools.rs` (sandbox dispatch)
  - `crates/temper-mcp/src/sandbox.rs` (authz error formatting)
  - `crates/temper-cli/src/serve/mod.rs` (observe auto-start)
  - `crates/temper-cli/src/decide/mod.rs` (terminal approval)

## Context

The first end-to-end governed external API call through Temper MCP (ADR-0007) succeeded but took ~7 attempts. Post-mortem revealed several UX issues:

1. **Agent self-approval**: The MCP sandbox exposed `approve_decision`, `deny_decision`, and `set_policy` methods, allowing agents to approve their own permissions. This defeats the purpose of Cedar governance.
2. **Opaque errors**: AuthorizationDenied errors didn't include decision IDs or guidance on what the agent should do next (wait for human approval).
3. **WASM result reading bug**: The engine reads `run()` return value as a memory pointer, but modules using `host_set_result` return 0 (or 1 on error), causing out-of-bounds memory access.
4. **No human approval channel**: Humans had no convenient way to see pending decisions and approve/deny them without using the agent's own sandbox.

## Decision

### Sub-Decision 1: Remove Governance Write Methods from Sandbox

Remove `approve_decision`, `deny_decision`, and `set_policy` from `dispatch_temper_method()` entirely. Since the Monty Python sandbox blocks ALL OS/network access, removing these methods from dispatch makes it truly impossible for agents to self-approve — not hidden, removed.

**Why this approach**: The sandbox is locked. The `temper` object methods are the only way the agent can interact with the server. Removing governance write methods from the dispatch layer provides a hard guarantee that agents cannot modify their own permissions. Read-only governance methods (`get_decisions`, `poll_decision`) remain so agents can check decision status.

### Sub-Decision 2: Two Human Approval Channels

Provide two channels for human decision approval:

1. **Observe UI** (browser): Universal channel. The `temper serve --observe` flag auto-starts the Next.js dev server alongside the Rust backend. When MCP's `start_server()` runs, it also starts the Observe UI and returns both URLs.

2. **`temper decide` CLI** (terminal): Developer-friendly channel. Connects to the server's SSE decision stream, shows pending decisions in the terminal, and accepts approve/deny input with scope selection.

A Claude Code PostToolUse hook detects AuthorizationDenied in MCP tool results and auto-opens the Observe UI decisions page.

**Why this approach**: Different developers prefer different workflows. Browser-based approval is universal; terminal-based approval is convenient for developers already in a terminal. The hook provides automatic bridging.

### Sub-Decision 3: Agent Wait Mechanism via poll_decision

When an action is denied, the enhanced error message includes:
- The decision ID (e.g., PD-abc123)
- Instruction to use `await temper.poll_decision(tenant, 'PD-abc123')` to wait
- The Observe UI URL for human reference

`poll_decision` polls the decisions API until the decision is no longer Pending (approved/denied), with a 30-second timeout. This gives the agent a structured way to wait for human approval without self-approving.

**Why this approach**: The agent needs a way to wait for resolution. Polling is simple, requires no WebSocket support in the sandbox, and the 30-second timeout prevents indefinite hangs.

### Sub-Decision 4: Fix WASM Result Reading Priority

Check `store.data().result_json` (set by `host_set_result`) first. Only fall back to memory pointer reading if host state is empty. This prioritizes the explicit host API over the legacy memory convention.

**Why this approach**: Modules using `host_set_result` return 0 from `run()`, which the current code interprets as "no result, check host state" — but only after trying the memory pointer path for non-zero values. A module returning 1 (error indicator) triggers an out-of-bounds memory read. Checking host state first is the correct priority.

## Rollout Plan

1. **Phase 0 (This PR)** — All changes ship together:
   - WASM engine fix, governance method removal, enhanced errors
   - `--observe` flag, `temper decide` CLI, PostToolUse hook
   - http_fetch hardening

2. **Phase 1 (Follow-up)** — Observe UI decisions page improvements:
   - Real-time SSE streaming of decision updates
   - Scope selection UI with preview of generated Cedar policy

## Consequences

### Positive
- Agents cannot self-approve permissions (hard guarantee via method removal)
- Humans have two convenient approval channels (browser + terminal)
- Enhanced error messages guide agents to wait, not self-fix
- WASM modules using `host_set_result` work correctly

### Negative
- Existing agent code using `approve_decision`/`deny_decision`/`set_policy` will break (intentional — these methods should never have been agent-accessible)
- Tests using agent self-approval flow need updating

### Risks
- Observe UI auto-start depends on `npm`/`node_modules` in the `observe/` directory — graceful fallback if unavailable
- `temper decide` SSE connection may timeout on slow networks — reconnection logic needed

## Non-Goals

- WebSocket-based real-time approval (too complex for sandbox)
- Multi-agent delegation of approval authority
- Automated approval policies (all approvals are human-initiated)

## Alternatives Considered

1. **Hide methods instead of removing** — Keep `approve_decision` in dispatch but return error. Rejected: removal is cleaner and provides a harder guarantee.
2. **WebSocket for poll_decision** — Real-time push instead of polling. Rejected: Monty sandbox doesn't support WebSocket connections; polling is simpler.
3. **Slack/Discord integration for approval** — Notify humans via messaging. Rejected: too much external dependency for Phase 0; good for future Phase 2.
