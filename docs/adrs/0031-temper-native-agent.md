# ADR-0031: Temper-Native Agent — Spec-Driven Agent Loop via IOA + WASM

- Status: Accepted
- Date: 2026-03-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0029: TemperFS — A Governed File System on Temper Primitives (conversation storage)
  - ADR-0026: Background Agent Capabilities (agent identity, adapter dispatch)
  - ADR-0019: Agentic Filesystem Navigation (entity graph navigation)
  - `.vision/AGENT_OS.md` (Temper as agent operating layer)
  - `crates/temper-wasm/` (WASM integration runtime, host functions)
  - `crates/temper-server/src/state/dispatch/` (adapter + WASM dispatch pipeline)
  - `crates/temper-server/src/secrets/` (secrets vault for LLM tokens)
  - `os-apps/temper-fs/` (TemperFS for conversation storage)

## Context

Temper's current agent-orchestration app (`os-apps/agent-orchestration/`) treats agents as opaque external processes — Claude Code CLI, Codex CLI — dispatched via Rust adapters that spawn local processes. This has two fundamental problems:

1. **Not deployable**: `tokio::process::Command` spawning a local CLI binary doesn't work when Temper runs on Railway, Fly.io, or any containerized deployment. The agent binary isn't installed in the container.

2. **Not governed**: The agent's internal behavior — its tool calls, turn budget, conversation management — is a black box. Temper's entire value proposition (specs define behavior, platform enforces mechanically) is bypassed for the most important workload: agent execution.

The agent's behavior should be a spec, not a black box. The turn cycle should be a state machine, not a `while` loop in Rust. Tools should be integrations, not hardcoded function calls. Budget enforcement should be invariants, not hope.

### The Key Insight

Temper's dispatch pipeline already supports loops through callback chaining: an integration's `on_success` callback triggers an action on the entity → that action fires another integration → callback → loop. The agent turn cycle IS the state machine:

```
Thinking → (LLM WASM integration) → callback → ProcessToolCalls or RecordResult
ProcessToolCalls → Executing → (tool WASM integration) → callback → HandleToolResults
HandleToolResults → Thinking → (LLM WASM integration) → ...  ← THE LOOP
```

No Rust `while` loop needed. The platform's dispatch pipeline drives the iteration mechanically.

### Everything Is HTTP

In both local and deployed cases, agent tools don't run on the Temper server — they run in a **sandbox** (Modal, E2B, Daytona). Every tool is an HTTP call:
- `read` → `GET {sandbox_url}/v1/fs/file?path=...`
- `bash` → `POST {sandbox_url}/v1/processes/run`
- LLM call → `POST api.anthropic.com/v1/messages`

All HTTP. All doable from WASM via `host_http_call`. Zero new Rust adapters needed.

## Decision

### Sub-Decision 1: The Agent IS a Temper App (IOA Spec + CSDL + Cedar)

The `TemperAgent` entity is defined as an IOA spec with a state machine that mechanically drives the agent turn cycle:

```
States: Created → Provisioning → Thinking → Executing → Completed | Failed | Cancelled
```

**State variables** (metadata in entity state, NOT conversation content):
- `prompt`, `model`, `provider`, `tools_enabled` — configuration
- `conversation_file_id` — FK to TemperFS File entity (conversation JSON stored externally)
- `workspace_id` — FK to TemperFS Workspace (agent's file namespace)
- `pending_tool_calls` — ephemeral JSON, current turn's tool calls only
- `turn_count` (counter), `max_turns` — budget enforcement via guard
- `input_tokens`, `output_tokens` (counters), `cost_cents` — cost tracking
- `sandbox_url`, `sandbox_id` — sandbox connection
- `result`, `has_result`, `error_message` — outcome

**The turn cycle** is expressed as state transitions with integration triggers:
1. `Configure` → sets all config, creates TemperFS workspace + conversation file
2. `Provision` → triggers `provision_sandbox` WASM integration → callback: `SandboxReady`
3. `Think` → triggers `call_llm` WASM integration → callback: `ProcessToolCalls` OR `RecordResult`
4. `ProcessToolCalls` → triggers `run_tools` WASM integration → callback: `HandleToolResults`
5. `HandleToolResults` → guard: `turn_count < max_turns` → transitions to `Thinking` → `call_llm` fires again ← **THE LOOP**
6. `RecordResult` → `Completed`

**Why this approach**: Every aspect of agent behavior is expressed as spec — lifecycle, budget, stopping condition, tool list, authorization. The platform enforces it mechanically. No Rust `while` loop, no coded agent logic.

### Sub-Decision 2: All Integrations Are WASM Modules

Three WASM modules, all using `host_http_call` for external communication:

**`llm_caller`**: Reads conversation from TemperFS (`host_http_call` GET), builds Anthropic Messages API request, calls LLM API (`host_http_call` POST), writes updated conversation back to TemperFS (`host_http_call` PUT), returns dynamic `callback_action` (`ProcessToolCalls` if tool_use blocks, `RecordResult` if end_turn).

**`tool_runner`**: Reads `pending_tool_calls` from invocation context, executes each tool via `host_http_call` to sandbox API (read/write/edit/bash), aggregates results, returns `callback_action = "HandleToolResults"`.

**`sandbox_provisioner`**: Provisions a Modal sandbox via `host_http_call` to Modal API, creates TemperFS Workspace + conversation File, returns `callback_action = "SandboxReady"` with sandbox_url, sandbox_id, workspace_id, conversation_file_id.

**Why WASM, not Rust adapters**: (1) WASM modules are hot-reloadable — change LLM provider or tool behavior without recompiling the server. (2) WASM modules run sandboxed with fuel/memory limits. (3) Everything is HTTP calls to external services — WASM's `host_http_call` handles this perfectly. (4) Same pattern as TemperFS's blob_adapter — proven architecture.

### Sub-Decision 3: Conversation Storage in TemperFS

Conversation history (which grows to 100K+ tokens) is stored as a **TemperFS File entity**, not inline in entity state. The `conversation_file_id` state variable holds the FK.

Each turn, the `llm_caller` module:
1. `host_http_call` → `GET /Files('{conversation_file_id}')/$value` (read current conversation)
2. Appends LLM response + tool results
3. `host_http_call` → `PUT /Files('{conversation_file_id}')/$value` (write updated conversation)

**Why TemperFS**: (1) Avoids event journal bloat — only metadata (file ID, turn count) in entity events, not 100KB conversation blobs. (2) Automatic versioning — each PUT creates a FileVersion, giving full turn-by-turn history. (3) Content-addressable dedup. (4) Cedar-governed access. (5) Already designed for this use case (ADR-0029 explicitly mentions agent artifact storage).

### Sub-Decision 4: LLM Authentication via Secrets Vault

The `llm_caller` integration config references the API key as `api_key = "{secret:anthropic_api_key}"`. At dispatch time, `resolve_secret_templates()` replaces this with the plaintext token from the secrets vault.

For Claude Max accounts: `claude setup-token` generates a long-lived OAuth token (~1 year), stored via `PUT /api/tenants/{tenant}/secrets/anthropic_api_key`. Encrypted at rest with AES-256-GCM.

**Why secrets vault**: Already exists, already integrated with WASM dispatch pipeline, already Cedar-governed, already encrypted. No new infrastructure needed.

### Sub-Decision 5: Modal as Sandbox Provider

Phase 0 targets Modal Sandbox API for sandbox provisioning. Modal provides sub-second sandbox creation, file system and process execution APIs, and GPU support for future use.

The sandbox is required even for local development — the architecture is uniform (always HTTP to sandbox API). No "local-only" fallback that bypasses the sandbox.

**Why no local fallback**: A local-only code path would create a deployment divergence. Same WASM modules, same HTTP calls, same specs — the only difference is the sandbox URL in the integration config.

### Sub-Decision 6: Dynamic Callback Routing for Branching

The `llm_caller` WASM module returns different `callback_action` values based on the LLM response:
- Tool use blocks → `callback_action = "ProcessToolCalls"`
- End turn → `callback_action = "RecordResult"`

The integration spec omits `on_success`, so the dispatch pipeline uses the WASM module's `callback_action` field (existing behavior in `dispatch_adapter_integrations_internal` — `integration.on_success.or_else(|| result.callback_action)`).

**Why dynamic callbacks**: The LLM's response determines the next action — this can't be known at spec time. Dynamic callbacks let the WASM module decide the next step based on runtime data, while the state machine still governs which transitions are valid.

## Rollout Plan

1. **Phase 0 (This PR)** — ADR, `temper_agent.ioa.toml` spec + CSDL + Cedar, three WASM modules (llm_caller, tool_runner, sandbox_provisioner), TemperFS integration, OS app bundle, E2E verification with Modal sandbox
2. **Phase 1** — OpenAI provider support, conversation compaction for long sessions
3. **Phase 2** — Daytona/E2B providers, agent-to-agent spawning, SSE streaming
4. **Phase 3** — Deployed parity testing, BudgetLedger integration, WideEvent telemetry per turn

## Readiness Gates

- `temper_agent.ioa.toml` passes L0-L3 verification cascade
- CSDL matches IOA params for all bound actions
- Full turn loop (Thinking → Executing → Thinking → Completed) verified E2E with running Temper
- LLM actually called (verified via conversation content in TemperFS)
- Tools actually execute in sandbox (verified via sandbox filesystem changes)
- Cedar denies unauthorized actions
- Budget enforcement works (max_turns=2 stops agent after 2 turns)

## Consequences

### Positive
- Agent behavior is fully spec-driven — lifecycle, budget, tools, authorization all mechanical
- No Rust `while` loop — platform dispatch pipeline drives the agent turn cycle
- Hot-reloadable — change LLM provider, tool behavior, or sandbox config without recompiling
- TemperFS provides versioned conversation history with Cedar access control
- Same architecture local and deployed — no divergence
- Validates that Temper primitives are expressive enough for complex agent workflows
- Existing secrets vault, dispatch pipeline, and WASM engine are reused — minimal new infrastructure

### Negative
- Requires TemperFS OS app installed as a dependency (conversation storage)
- Requires Modal account for sandbox provisioning (even locally)
- WASM module development is less ergonomic than writing Rust directly
- Conversation read/write adds HTTP round-trips per turn (mitigated by TemperFS caching)

### Risks
- **WASM module complexity**: LLM API parsing and tool execution in WASM may hit edge cases with `host_http_call` (large responses, timeouts). Mitigation: increase `max_duration` and `max_response_bytes` for agent modules.
- **Callback chain depth**: Long agent sessions create deep callback chains. Mitigation: each callback is a fresh dispatch — no stack growth, no recursion.
- **Modal API changes**: Modal's sandbox API is not fully stable. Mitigation: sandbox_provisioner WASM module is hot-reloadable — update without recompiling server.
- **TemperFS not yet implemented**: TemperFS is defined but may not be fully deployed. Mitigation: TemperFS implementation is a prerequisite; if blocked, fall back to inline conversation storage temporarily.

### DST Compliance

All new code lives in WASM modules (not simulation-visible) and OS app specs (IOA/CSDL/Cedar). No changes to temper-runtime, temper-jit, or the core dispatch pipeline. The existing dispatch pipeline is already DST-compliant.

WASM modules use `host_http_call` (async I/O handled by host, not WASM) and are marked `// determinism-ok: WASM integration side-effects run outside simulation core`.

## Non-Goals

- **Replacing the existing agent-orchestration app** — HeartbeatRun/Organization entities continue to work for external agent dispatch. TemperAgent is an additional option for spec-driven agents.
- **Building a general-purpose agent framework** — This is specifically a Temper-native agent. External agent frameworks (LangGraph, CrewAI) remain compatible via adapters.
- **FUSE/filesystem mount** — Sandbox tools use HTTP API, not filesystem operations.
- **Streaming LLM responses** — Phase 0 uses request/response. SSE streaming is Phase 2.
- **Multi-model orchestration** — Single model per TemperAgent. Multi-model routing is future work.

## Alternatives Considered

1. **Rust crate with coded agent loop (`temper-agent`)** — A Rust binary with a `while` loop calling LLM API and executing tools. Rejected because it bypasses Temper's spec-driven philosophy. The agent loop IS the state machine — coding it in Rust defeats the purpose.

2. **Three new Rust adapters (LlmAdapter, ToolRunnerAdapter, SandboxAdapter)** — Native Rust adapters registered in AdapterRegistry. Rejected because (a) not hot-reloadable, (b) all operations are HTTP calls suitable for WASM, (c) adds permanent Rust code for what should be app-level logic.

3. **Inline conversation storage in entity state** — Store full conversation JSON as a string state variable. Rejected because conversations grow to 100K+ tokens, bloating the event journal. TemperFS provides external storage with versioning and dedup.

4. **Local-only mode without sandbox** — Run tools directly on the host machine for local development. Rejected because it creates a deployment divergence — different code paths local vs deployed.

## Rollback Policy

The temper-agent OS app lives in `os-apps/temper-agent/` and can be unregistered from the platform without affecting other apps. WASM modules are loaded on demand. No changes to core framework code — rollback is removing the OS app directory and its platform registration.
