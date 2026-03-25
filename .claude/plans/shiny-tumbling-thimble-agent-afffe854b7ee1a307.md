# OpenClaw Clean Core Architecture

## Overview

OpenClaw is a TypeScript/Node.js monorepo (pnpm workspaces) that acts as a **personal AI assistant gateway**. It connects 23+ messaging channels (Discord, Telegram, WhatsApp, Slack, etc.) to LLM backends (Anthropic Claude, OpenAI, etc.) through a WebSocket control plane. The core is NOT a WASM sandbox system -- it is a local-first Node.js process with optional Docker sandboxing for non-main sessions.

---

## 1. The Gateway (Control Plane)

**Location:** `src/gateway/`

The Gateway is a **WebSocket server** bound to `ws://127.0.0.1:18789`. It is the central coordination hub.

### Boot Sequence
- **`src/gateway/boot.ts`** -- Reads `BOOT.md` from workspace, constructs a prompt, runs the agent once in an isolated session, then restores the main session mapping. Uses `generateBootSessionId()` (timestamp + truncated UUID).

### Connection Model
- **`src/gateway/client.ts`** -- `GatewayClient` class manages WebSocket connections with:
  - **Authentication**: Multiple auth methods (token, bootstrap token, device token, password, signature token). Device identity signing. TLS fingerprint validation.
  - **Protocol**: `RequestFrame` / `ResponseFrame` for RPC, `EventFrame` for push events with sequence tracking and gap detection.
  - **Reconnection**: Exponential backoff (1s to 30s). Tick-based liveness detection (stall timeout = 2x tick interval).
  - **Security**: Blocks plaintext `ws://` to non-loopback addresses (CWE-319).

### RPC Dispatch
- **`src/gateway/call.ts`** -- `callGateway()` is the primary RPC entry point. Routes to:
  - `callGatewayCli()` -- Default CLI operator scopes
  - `callGatewayLeastPrivilege()` -- Minimum required scopes for the method
  - `callGatewayScoped()` -- Explicit user-supplied scopes
  - `executeGatewayRequestWithScopes()` -- Creates `GatewayClient`, handles hello handshake, executes RPC method, manages timeout/close events.

### Key Gateway Methods (RPC)
- `"agent"` -- Dispatch a message to an agent for processing
- `"sessions.patch"` -- Create/update session metadata
- Channel-specific methods for sending, reactions, etc.

### Webhook Hooks
- **`src/gateway/hooks.ts`** -- HTTP webhook ingress with:
  - `HookAgentPayload { message, channel, sessionKey?, agentId?, ... }`
  - Token auth via `Authorization: Bearer` or `x-openclaw-token` header
  - Session key resolution (hook-prefixed UUID if not provided)
  - Agent allowlist enforcement

---

## 2. Agent Runtime (NOT a WASM sandbox -- it's CLI-based)

**Location:** `src/agents/`

There is NO WASM sandbox. OpenClaw does NOT have an "Agent Runtime Engine" in the traditional sense. Instead, agents are **CLI processes** (Claude Code, Codex, Pi, OpenCode) spawned as child processes.

### Agent Identity & Scope
- **`src/agents/agent-scope.ts`** -- Core agent resolution:
  ```typescript
  type ResolvedAgentConfig = {
    name?: string;
    workspace?: string;
    agentDir?: string;
    model?: AgentEntry["model"];
    skills?: AgentEntry["skills"];
    identity?: AgentEntry["identity"];
    sandbox?: AgentEntry["sandbox"];
    tools?: AgentEntry["tools"];
    // ... heartbeat, subagents, groupChat, etc.
  };
  ```
  - `resolveDefaultAgentId(cfg)` -- First agent with `default=true`, or first in list, or `DEFAULT_AGENT_ID`
  - `resolveSessionAgentId({ sessionKey, config })` -- Parses agent ID from session key format `agent:<agentId>:...`
  - `resolveAgentWorkspaceDir(cfg, agentId)` -- Per-agent workspace directories
  - `resolveAgentSkillsFilter(cfg, agentId)` -- Per-agent skill allowlists

### CLI Runner (How agents are actually executed)
- **`src/agents/cli-runner.ts`** -- `runCliAgent()`:
  1. Resolves workspace directories and backend config
  2. Prepares system prompt with bootstrap context
  3. Builds CLI arguments
  4. Executes via process supervisor with timeout handling
  5. Handles session management (new or resumed)
  6. Retry logic for expired sessions
  7. Image payload management
  8. Returns execution results with usage metrics

  `runClaudeCliAgent()` wraps this with provider="claude-cli", model="opus".

### ACP (Agent Control Protocol) Spawning
- **`src/agents/acp-spawn.ts`** -- `spawnAcpDirect()` for spawning isolated agent sessions:
  ```typescript
  type SpawnAcpParams = {
    task: string;
    label?: string;
    agentId?: string;
    mode?: "run" | "session";  // oneshot vs persistent
    thread?: boolean;           // bind to a channel thread
    sandbox?: "inherit" | "require";
    streamTo?: "parent";        // relay output to parent session
  };

  type SpawnAcpResult = {
    status: "accepted" | "forbidden" | "error";
    childSessionKey?: string;   // format: "agent:<agentId>:acp:<uuid>"
    runId?: string;
    mode?: SpawnAcpMode;
  };
  ```
  Flow:
  1. Check ACP policy (`isAcpEnabledByPolicy`)
  2. Resolve target agent ID from config
  3. Create session key: `agent:{agentId}:acp:{uuid}`
  4. Register session via `callGateway({ method: "sessions.patch" })`
  5. Initialize runtime via `getAcpSessionManager().initializeSession()`
  6. Optionally bind to a channel thread (Discord thread, etc.)
  7. Dispatch task via `callGateway({ method: "agent", params: { message, sessionKey, ... } })`

### Sandbox Model
- **Main session**: Runs on host with full tool access
- **Non-main sessions**: Can run sandboxed in Docker containers
- `resolveSandboxRuntimeStatus({ cfg, sessionKey })` determines sandbox status
- Sandboxed sessions CANNOT spawn ACP sessions (host-only)

---

## 3. The SKILL.md System (NOT SOUL.md)

**Location:** `skills/` directory (56 skill directories)

OpenClaw uses `SKILL.md` files, NOT `SOUL.md`. Each skill is a directory containing a single `SKILL.md` markdown file that acts as both documentation and prompt injection.

### Skill Format (from `skills/coding-agent/SKILL.md`)
Skills are **plain markdown documents** with:
- Title and description
- Usage instructions (tool parameters, flags, examples)
- Rules and constraints
- The content is injected verbatim into the agent's system prompt

### Skill Loading Pipeline
- **`src/auto-reply/skill-commands.ts`**:
  1. `listSkillCommandsForWorkspace(workspaceDir, config)` -- Builds skill command specs with skill filters
  2. `listSkillCommandsForAgents(agentIds)` -- Iterates agents, resolves workspace dirs, deduplicates by canonical path
  3. `mergeSkillFilters()` -- Unrestricted (undefined) takes precedence; empty `[]` contributes nothing; non-empty arrays merge via dedup
  4. `dedupeBySkillName()` -- Lowercase normalization, preserves insertion order

### Skill Filtering (Per-Agent)
- **`src/agents/agent-scope.ts`**: `resolveAgentSkillsFilter(cfg, agentId)` reads `skills` from agent config
- Agent config supports per-agent skill allowlists:
  ```yaml
  agents:
    list:
      - id: "molty"
        skills: ["coding-agent", "discord", "github"]
      - id: "helper"
        skills: []  # no skills
  ```

### Skill Injection Point
- In `getReplyFromConfig()` (the main reply pipeline):
  1. Skill filters merged from channel + agent settings
  2. Passed through chain via `skillFilter` parameter
  3. Injected during directive resolution and inline action handling
  4. SKILL.md content becomes part of system prompt context

### Available Skills (56 total)
Key ones: `coding-agent`, `discord`, `slack`, `github`, `gh-issues`, `obsidian`, `notion`, `canvas`, `weather`, `spotify-player`, `voice-call`, `tmux`, `trello`, `camsnap`, etc.

---

## 4. Discord Integration -- The Clean Path

**Location:** `extensions/discord/` (plugin) + `skills/discord/` (skill)

### Plugin Registration
```typescript
// extensions/discord/index.ts
export default defineChannelPluginEntry({
  id: "discord",
  name: "Discord",
  description: "Discord channel plugin",
  plugin: discordPlugin,
  setRuntime: setDiscordRuntime,
  registerFull: registerDiscordSubagentHooks,
});
```

### Plugin Registry
- **`src/channels/registry.ts`** -- Global plugin registry via `Symbol.for("openclaw.pluginRegistryState")`
- Plugins register at startup, keyed by ID with optional aliases
- `normalizeAnyChannelId()` and `findRegisteredChannelPluginEntry()` for lookup

### Discord Channel Plugin (`extensions/discord/src/channel.ts`)
`discordPlugin` is a `ChannelPlugin<ResolvedDiscordAccount, DiscordProbe>` created via `createChatChannelPlugin()` with:
- Allowlist management (legacy DM account support)
- Group policy resolution
- Mention stripping patterns
- Agent prompt hints for Discord components/forms
- Message normalization and target parsing
- Outbound: text (2000 char limit), media attachments, polls, silent delivery

### Discord Monitor Provider (`extensions/discord/src/monitor/provider.ts`)
`monitorDiscordProvider()` orchestrates the Discord bot lifecycle:
1. Load Discord account settings, thread bindings, feature flags
2. Deploy native slash commands (with retry + rate-limit handling)
3. Register interactive components (buttons, select menus, modals)
4. Register event listeners (messages, reactions, threads, presence)
5. Optionally initialize voice channel management
6. Manage WebSocket gateway connection + reconnection

### Discord Message Handler (`extensions/discord/src/monitor/message-handler.ts`)
`createDiscordMessageHandler()` returns a handler with `deactivate()`:
1. **Dedup**: LRU cache (5-min TTL, 5000 max entries)
2. **Debounce**: Batches consecutive messages from same author in same channel
3. **Bot filter**: Filters out bot's own messages early
4. **Batch**: Creates synthetic concatenated message for multi-message bursts
5. **Config resolution**: Merges Discord-specific config with channel defaults

### Discord Event Listeners (`extensions/discord/src/monitor/listeners.ts`)
- `DiscordMessageListener` -- Fire-and-forget delegation to handler
- `DiscordReactionListener` / `DiscordReactionRemoveListener` -- Authorization checks + notification emit
- `DiscordPresenceListener` -- Caches user presence data
- `DiscordThreadUpdateListener` -- Closes sessions when threads archive
- `runDiscordListenerWithSlowLog()` -- 30s slow-log wrapper

### The Clean Path: Discord Message -> Agent Response

```
1. Discord WebSocket Gateway receives MESSAGE_CREATE
   |
2. DiscordMessageListener.onMessage(message)
   |  (fire-and-forget, no blocking)
   |
3. createDiscordMessageHandler() processes:
   a. Filter bot's own messages
   b. Check dedupe cache (5-min TTL)
   c. Enqueue into debouncer (batch consecutive msgs from same author)
   d. Debouncer flushes -> single or synthetic batched message
   |
4. Message enters Channel Session Layer (src/channels/):
   a. session-envelope.ts: resolveInboundSessionEnvelopeContext()
   b. session.ts: recordInboundSession() -- normalize session key, update last route
   c. mention-gating.ts: check if bot was mentioned (for group chats)
   d. command-gating.ts: check if message is a command
   |
5. Routing (src/routing/):
   resolveAgentRoute(input) resolves which agent handles this message:
   - Priority tiers: peer > parent peer > guild+roles > guild > team > account > channel
   - Returns: { agentId, sessionKeys, routingPolicy }
   - Session key format: "agent:<agentId>:<channel>:<normalized-peer>"
   |
6. Run State Machine (src/channels/run-state-machine.ts):
   createRunStateMachine() tracks active runs:
   - onRunStart() increments counter, publishes busy status
   - Heartbeat interval (60s) for long-running operations
   - onRunEnd() decrements, clears heartbeat when idle
   |
7. Dispatch (src/auto-reply/dispatch.ts):
   dispatchInboundMessage(ctx, cfg, dispatcher)
   -> dispatchReplyFromConfig()
   -> withReplyDispatcher() (ensures cleanup on all exit paths)
   |
8. Reply Pipeline (src/auto-reply/reply/get-reply.ts):
   getReplyFromConfig() -- THE MAIN ORCHESTRATOR:
   a. Resolve agent identity (resolveSessionAgentId)
   b. Merge skill filters (channel + agent level)
   c. Establish workspace directory + bootstrap files
   d. Resolve model selection (default / heartbeat override / channel override)
   e. Finalize inbound context
   f. Apply media understanding (if media detected)
   g. Apply link understanding (if URLs detected)
   h. Emit pre-agent-message hooks
   i. Initialize session state
   j. Resolve command authorization
   k. Parse directives (commands, model switches, etc.)
   l. Handle inline actions (skill invocation, commands, elevation)
   m. Stage sandbox media files
   n. runPreparedReply() -- actually calls the LLM
   |
9. Context Engine (src/context-engine/):
   ContextEngine.assemble() builds the model context:
   - Ordered messages within token budget
   - System prompt additions
   - Token estimates
   |
10. Agent Execution:
    CLI runner spawns the actual agent process (Claude, Codex, etc.)
    OR
    Direct API call to LLM provider
    |
11. Response flows back through:
    ReplyPayload -> dispatcher -> channel outbound adapter
    -> Discord send.ts -> Discord API (message create/edit)
```

---

## 5. Tool System

**Location:** `src/agents/bash-tools.*`, `src/agents/channel-tools.ts`, skill SKILL.md files

### Tool Categories
1. **Bash tools** (`bash-tools.exec.ts`, `bash-tools.process.ts`, `bash-tools.shared.ts`):
   - Shell execution with PTY support
   - Background mode with session tracking
   - Process supervision (poll, log, write, submit, send-keys, kill)
   - Docker exec for sandboxed sessions
   - Exec approval workflow for sensitive commands

2. **Channel tools** (`channel-tools.ts`):
   - `message` tool -- Send messages to any channel
   - `process` tool -- Manage background processes
   - Channel-specific operations (reactions, edits, deletes)

3. **Browser tools** (`src/browser/`):
   - CDP-managed Chrome instance
   - Page navigation, screenshots, interaction

4. **Canvas tools** (`src/canvas-host/`):
   - A2UI push/reset for visual workspace

5. **Node tools** (`src/node-host/`):
   - Camera, screen recording, system commands
   - Device-specific operations

6. **Skill-provided tools**:
   - Each SKILL.md can describe tool usage patterns
   - Skills are prompt-injected, not programmatic tool registrations

### Tool Dispatch Pattern
Tools are NOT a formal registry with schemas. Instead:
- Core tools (bash, message, process, browser, canvas) are built into the agent runtime
- Skills inject knowledge about tool usage via system prompt
- The LLM decides which tools to call based on the combined prompt
- Tool results flow back through the `onToolResult` callback in `GetReplyOptions`

### Key Types
```typescript
type GetReplyOptions = {
  runId?: string;
  abortSignal?: AbortSignal;
  images?: ImageContent[];
  onAgentRunStart?: (runId: string) => void;
  onPartialReply?: (payload: ReplyPayload) => void;
  onBlockReply?: (payload: ReplyPayload, context?: BlockReplyContext) => void;
  onToolResult?: (payload: ReplyPayload) => void;
  onToolStart?: (payload: { name?: string; phase?: string }) => void;
  skillFilter?: string[];
  // ... typing, compaction, model selection callbacks
};

type ReplyPayload = {
  text?: string;
  mediaUrl?: string;
  mediaUrls?: string[];
  interactive?: InteractiveReply;
  replyToId?: string;
  isError?: boolean;
  isReasoning?: boolean;
  channelData?: Record<string, unknown>;
};
```

---

## 6. Coding Agent Integration

**Location:** `skills/coding-agent/SKILL.md`

Coding agents are integrated as **bash tool invocations**, NOT as first-class runtime primitives. The `coding-agent` skill teaches the AI how to spawn and manage them.

### Supported Agents
| Agent | Invocation | Notes |
|-------|-----------|-------|
| **Codex** | `bash pty:true command:"codex exec 'prompt'"` | Requires git repo, PTY essential |
| **Claude Code** | `bash command:"claude --permission-mode bypassPermissions --print 'task'"` | No PTY needed, `--print` mode |
| **OpenCode** | `bash pty:true command:"opencode run 'task'"` | PTY required |
| **Pi** | `bash pty:true command:"pi 'task'"` | PTY required |

### Execution Modes
1. **Foreground**: Direct invocation, blocks until complete
2. **Background**: `background:true`, returns `sessionId` for monitoring via `process` tool
3. **Parallel**: Multiple background sessions for batch work (PR reviews, issue fixes)

### Auto-Notify Pattern
```bash
# Append wake trigger for completion notification:
openclaw system event --text "Done: [summary]" --mode now
```

### Workspace Isolation
- `workdir` parameter ensures agent operates in correct directory
- Git worktrees for parallel issue fixing
- NEVER start coding agents in `~/.openclaw/` (reads soul docs) or the live OpenClaw instance dir

---

## Key Architectural Observations

1. **No WASM, no formal agent runtime engine**: Agents are CLI processes spawned via `child_process`. The "runtime" is session management + process supervision.

2. **Gateway is the brain**: Everything routes through the WebSocket control plane. Even internal agent-to-agent communication uses `callGateway()`.

3. **Skills are prompt injection**: SKILL.md files are loaded, filtered per-agent, and injected into the system prompt. No formal tool schema registration -- the LLM infers tool usage from the markdown.

4. **Channel plugins are the extension model**: Each channel (Discord, Slack, etc.) is a `ChannelPlugin` registered via `defineChannelPluginEntry()`. The plugin provides: message normalization, allowlists, outbound adapters, and event listeners.

5. **Session keys encode routing**: Format is `agent:<agentId>:<channel>:<peer>` or `agent:<agentId>:acp:<uuid>` for spawned sessions.

6. **Multi-agent via config**: Multiple agents are defined in config, each with their own workspace, skills filter, model, and identity. Routing bindings determine which agent handles which conversations.

7. **The reply pipeline is the core loop**: `getReplyFromConfig()` is the single function that orchestrates everything from message receipt to agent execution to response delivery.
