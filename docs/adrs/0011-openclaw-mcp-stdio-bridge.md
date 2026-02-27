# ADR-0011: OpenClaw MCP Stdio Bridge

- Status: Proposed
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0006: Spec-Aware Agent Interface for MCP
  - ADR-0007: Governed External API Calls Through MCP
  - ADR-0009: WASM Developer Experience
  - `.vision/AGENT_OS.md` (REPL interface as primary agent surface)
  - `plugins/openclaw-temper/index.ts` (current plugin)
  - `crates/temper-mcp/src/` (MCP server implementation)

## Context

The OpenClaw plugin currently wraps Temper's OData HTTP API as a single tool with
five fixed operations (list, get, create, action, patch). This limits OpenClaw agents
to CRUD on pre-existing entity types. They cannot:

- Generate and submit IOA specs dynamically
- Run the L0-L3 verification cascade
- Manage Cedar policies
- Handle authorization denials with `poll_decision`
- Compile and upload WASM integration modules
- Inspect loaded specs programmatically

Meanwhile, the Temper MCP server (`temper mcp`) already provides all of this through
two tools — `search` (read-only spec queries) and `execute` (full governed operations)
— exposed over stdio JSON-RPC 2.0. Claude Code uses this interface today via `.mcp.json`.

The gap: OpenClaw agents have no access to the MCP REPL. The plugin speaks HTTP
directly instead of bridging through the governed sandbox.

## Decision

### Sub-Decision 1: MCP Stdio Bridge in the Plugin

Replace the HTTP-based `temper` tool with an `McpStdioBridge` class that spawns
`temper mcp` as a child process and communicates over stdin/stdout JSON-RPC 2.0.

The bridge:
- Spawns on service `start()`, kills on `stop()`
- Sends `initialize` + `notifications/initialized` handshake
- Exposes `callTool(name, code)` that sends `tools/call` requests
- Matches responses to pending promises by integer request ID
- Auto-restarts lazily on next tool call if subprocess crashes

**Why this approach**: Zero npm dependencies. The JSON-RPC protocol surface is tiny
(initialize, tools/call, response parsing). Raw implementation is ~100 lines. Using
the `@modelcontextprotocol/sdk` would add a dependency to a currently zero-dep plugin
for marginal benefit.

### Sub-Decision 2: Two Tools Replace One

Register `temper_search` and `temper_execute` as separate OpenClaw agent tools,
each taking a single `code` string parameter. The tool descriptions include the
available Python API methods so the agent knows what to call.

**Why two tools**: Matches the MCP server's native tool split. `search` works without
a running server (spec inspection only). `execute` requires a server. Keeping them
separate lets the agent query specs without starting the runtime.

### Sub-Decision 3: Keep SSE Service Alongside Bridge

The MCP protocol is request-response only — no event streaming. The existing SSE
service (connects to `/tdata/$events`, writes signal files, posts `/hooks/wake`)
remains unchanged. It provides the real-time event bridge that wakes agents on
Temper state changes.

**Why not replace SSE**: MCP has no subscription/streaming mechanism. SSE is the only
way to push events to the agent without polling.

### Sub-Decision 4: Add `--agent-id` to `temper mcp` CLI

The CLI currently hardcodes `principal_id: Some("mcp-agent")`. Add an optional
`--agent-id` flag so the plugin can pass the actual OpenClaw agent identity.

Temper has two identity systems that overlap:
- **`AgentContext`** (`X-Agent-Id`) — observability: trajectory logs, OTEL, audit trail
- **`SecurityContext`** (`X-Temper-Principal-Id`) — authorization: Cedar policy evaluation

The MCP server sends `X-Temper-Principal-Id`, and `extract_agent_context()` in
`dispatch.rs` falls back to it when `X-Agent-Id` is absent. So one value covers both.

The flag is named `--agent-id` (not `--principal-id`) because that's the intuitive
name for OpenClaw users. Internally it maps to `McpConfig.principal_id`.

**Why a CLI flag**: The agent ID is set at process startup and doesn't change.
A CLI arg is simpler than an environment variable or runtime RPC method.

### Sub-Decision 5: Context Injection via `before_prompt_build`

Use OpenClaw's `before_prompt_build` plugin hook to inject a compact summary of
loaded Temper tenants and entity types at the start of each agent turn. This uses
the `search` tool (no server needed) so it's lightweight.

**Why**: Gives the agent ambient awareness of what Temper apps are loaded without
requiring it to call a tool first. Aligns with the "Temper as OS" feel.

## Rollout Plan

1. **Phase 0 (This PR)**
   - Add `--agent-id` to `temper mcp` CLI (3 lines of Rust)
   - Rewrite plugin: McpStdioBridge class, two new tools, `before_prompt_build` hook
   - Update `openclaw.plugin.json` config schema
   - Remove dead HTTP tool code
   - End-to-end test with real `temper mcp` subprocess

2. **Phase 1 (Follow-up)**
   - Update `skills/temper-openclaw/SKILL.md` to document the new tools
   - Add `before_tool_call` interception (optional governance enforcement)
   - Consider cron-based periodic Temper state summaries

## Consequences

### Positive
- OpenClaw agents get the full Temper REPL — spec submission, verification,
  governance, WASM compilation — same as Claude Code
- Agents can dynamically build any app (email, task management, etc.) through
  conversation, not pre-built operations
- Cedar governance applies to all agent operations (default-deny, human approval)
- Zero new dependencies — plugin stays at `"dependencies": {}`
- SSE real-time events continue working unchanged

### Negative
- Requires `temper` binary on PATH or configured path (not just an HTTP URL)
- MCP subprocess adds a process to manage (crash recovery, lifecycle)
- 180-second sandbox timeout means long `poll_decision` waits can time out

### Risks
- **Subprocess stability**: If `temper mcp` crashes frequently, the lazy restart
  pattern may cause tool call failures. Mitigation: exponential backoff on restarts.
- **OpenClaw plugin API stability**: `before_prompt_build` hook is documented but
  could change between OpenClaw versions. Mitigation: graceful degradation (skip
  injection if hook unavailable).

## Non-Goals

- Patching OpenClaw core (everything uses the public plugin API)
- Replacing the SSE event service (MCP can't stream events)
- Tool call interception / governance enforcement via `before_tool_call` (Phase 1)
- Custom memory backend integration (future consideration)

## Alternatives Considered

1. **Extend the HTTP tool with more operations** — Add `submit-specs`, `decisions`,
   etc. as new operations on the existing tool. Rejected: this duplicates the MCP
   API surface in TypeScript and doesn't give agents the REPL sandbox. Every new
   MCP method would need a corresponding HTTP wrapper.

2. **Use @modelcontextprotocol/sdk** — Official TypeScript MCP client. Rejected:
   adds an npm dependency for a protocol surface of 3 message types. The raw
   implementation is simpler and keeps the plugin at zero deps.

3. **Wait for OpenClaw native MCP support** — OpenClaw's MCP integration is
   incomplete (issues #4834, #13248). Rejected: timeline unknown, and the plugin
   bridge pattern works today.

## Rollback Policy

Revert to the previous HTTP-based plugin. The `temper` tool with 5 operations
still works against any running Temper server. Git revert of the plugin files
restores full functionality.
