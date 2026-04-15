# ADR-0033: Sandbox Fsync to TemperFS

- Status: Accepted
- Date: 2026-03-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0029: TemperFS (workspace, file, blob storage)
  - ADR-0031: Temper-native agent (IOA spec-driven agent loop)
  - `os-apps/temper-agent/wasm/tool_runner/src/lib.rs`
  - `os-apps/temper-fs/wasm/blob_adapter/src/lib.rs`

## Context

The temper-agent has a working agent loop with conversation stored in TemperFS. However, **sandbox filesystem state is ephemeral** — if the sandbox dies, the E2B session expires, or the agent is interrupted, all files created/modified by tool calls are lost. This blocks two critical capabilities:

- **Resume**: restart an interrupted agent from where it left off (restore sandbox files + conversation)
- **Replay**: reconstruct exact agent state at any turn for debugging/auditing

The sandbox filesystem is the primary work product of the agent (code files, configs, scripts). Losing it means losing the agent's work output.

## Decision

### Sub-Decision 1: Sandbox-as-Source-of-Truth

Use the sandbox's own bash execution to enumerate workspace files after each tool batch, rather than tracking file paths in tool_runner code.

**Why this approach**: Path tracking in tool_runner would miss files created by bash commands, subprocesses, or any indirect means. The sandbox already has a bash execution API — running `find` gives a complete, authoritative file listing regardless of how files were created. This also naturally detects deletions by diffing against the previous manifest.

### Sub-Decision 2: Stat-Based Change Detection (rsync algorithm)

Combine `find` with `stat` to get `(path, size_bytes, mtime)` per file in a single bash call. Compare against the stored manifest to determine which files actually changed:

- **New file** (in sandbox but not manifest): read + upload
- **Changed file** (size or mtime differs): read + upload
- **Unchanged file** (size AND mtime match): skip entirely
- **Deleted file** (in manifest but not sandbox): archive TemperFS entity

CAS dedup in blob_adapter provides a safety net — even if mtime changes without content change, the SHA256 hash match prevents redundant blob storage.

**Why this approach**: Without stat metadata, every sync would read ALL files from the sandbox. With stat, a typical turn touching 1 of 20 files reads only 1 file instead of 20.

### Sub-Decision 3: Sync Every Tool Batch (not periodically)

Fsync runs after every tool batch, not periodically or only at terminal states.

**Why this approach**: The whole point is durability. Syncing every N turns loses up to N-1 turns on crash. Syncing only at completion loses all file state if the agent is interrupted. The `find` + `stat` command costs <50ms on typical workspaces (5-50 files), while the agent turn cycle is 10-30s. Overhead is <0.5% of turn time.

### Sub-Decision 4: Best-Effort Sync

Fsync failures log warnings but do NOT fail the HandleToolResults transition. The agent loop continues even if TemperFS is temporarily unavailable. Conversation storage (already working) takes priority.

### Sub-Decision 5: Resume via IOA Action + WASM Integration

A new `Resume` action transitions Created → Thinking, triggering a `restore_workspace` WASM integration that reads the manifest from TemperFS and restores files to a new sandbox before starting the LLM loop.

**Why this approach**: Resume is a spec-level concern, not a code-level hack. The IOA action makes it visible, governable (Cedar), and auditable.

### Sub-Decision 6: Replay is Free

TemperFS automatically creates FileVersion entities on each $value PUT (via reactions). Combined with conversation FileVersions, exact agent state at any turn can be reconstructed without additional code.

## Rollout Plan

1. **Phase 0 (This PR)** — IOA spec + CSDL changes, sandbox_provisioner manifest creation, tool_runner fsync, workspace_restorer module, E2E tests.
2. **Phase 1 (Follow-up)** — Large file support via `host_http_call_stream` SDK binding (host function already exists).
3. **Phase 2** — Replay UI in Observe dashboard.

## Consequences

### Positive
- Agent sandbox state preserved across sandbox restarts, E2B session expiry, and agent interruption
- Per-turn file state replay from TemperFS versioning — no additional code needed
- Resume capability enables long-running agents that survive infrastructure failures
- CAS dedup + stat-based change detection make re-sync nearly free

### Negative
- Files > 60KB skipped (WASM SDK 64KB response buffer limit) — clean upgrade path via streaming
- One additional bash command per tool batch (~50ms overhead)

### Risks
- `stat` format differs between Linux (GNU) and macOS (BSD) — mitigated by `is_e2b_sandbox()` branching
- Manifest can drift if fsync fails mid-sync — mitigated by best-effort design + CAS dedup on next successful sync

## Non-Goals

- Real-time file watching (inotify/fswatch) — requires sandbox API changes
- Binary file support (images, compiled artifacts) — text files only for now
- Cross-agent file sharing — each agent has its own workspace
- Streaming large files — deferred to Phase 1

## Alternatives Considered

1. **Track file paths in tool_runner** — Only catches write/edit tool calls, misses bash-created files. Against Temper philosophy of building through specs/integrations. Rejected.
2. **`find -newer` incremental scan** — Cannot detect deletions. Requires two commands (newer + full listing). Worse than single `find` + `stat`. Rejected.
3. **Sync only at terminal states** — Loses all file state on mid-execution crash. Defeats resume/replay purpose. Rejected.
4. **Sync every N turns** — Arbitrary parameter. Loses up to N-1 turns on crash. Complexity for marginal gain over every-batch sync. Rejected.
5. **inotify/fswatch in sandbox** — Neither E2B nor local sandbox expose file watching. Would require sandbox API changes. Rejected.
