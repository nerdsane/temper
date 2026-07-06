# ADR-0155: REPL host-op isolation and authorization gate

- Status: Accepted
- Date: 2026-07-06
- Deciders: Temper core maintainers
- Related:
  - ARN-166: `[CRITICAL] Kernel /api/repl is unauthenticated → arbitrary host file-read + RCE`
  - ARN-167: `[CRITICAL] TemperPaw auth bypass via self-asserted x-temper-* principal` (global header-trust; separate PR)
  - `crates/temper-sandbox/src/dispatch.rs` (shared `temper.*` dispatch)
  - `crates/temper-sandbox/src/repl.rs` (server REPL runner)
  - `crates/temper-mcp/src/runtime.rs` (local stdio MCP runner)
  - `crates/temper-server/src/api/repl.rs`, `crates/temper-server/src/api/mod.rs` (HTTP surface)

## Context

`POST /api/repl` executes agent-supplied Python in the Monty sandbox. The Python
can call `temper.*` methods, dispatched by the shared
`temper_sandbox::dispatch::dispatch_temper_method`. Two of those methods perform
**host-process** operations rather than looping back over HTTP:

- `temper.upload_wasm(name, path)` → `tokio::fs::read(path)` on an attacker-chosen
  path, then POSTs the bytes to `/api/wasm/modules/{name}`. Arbitrary host file
  read; no dependency on cwd or a resolved binary path, so it is live in every
  deployment including production.
- `temper.compile_wasm(name, src)` → writes attacker Rust to a temp crate and runs
  `cargo build`. Code execution as the server user via `build.rs`/proc-macros,
  gated today only by the server's cwd happening to be a temper workspace.

Two facts make this exploitable:

1. **`dispatch_temper_method` is context-blind.** It is called by exactly two
   runners — the server REPL (`temper-sandbox/src/repl.rs`, whose host process is
   the **Temper server**) and `temper-mcp` (`temper-mcp/src/runtime.rs`, a
   **stdio** server the developer runs on their **own machine**). Host filesystem
   and `cargo build` operations are legitimate only for the second: they act on
   the developer's local checkout. For the first, they act on the server host —
   which is never the intent.

2. **`/api/repl` has no authorization.** The route is registered with no auth
   extractor, and `handle_repl` reads the principal straight from the
   self-asserted `x-temper-principal-id` header. Anyone who can reach the port can
   execute REPL code — fully open on a standalone kernel with no `TEMPER_API_KEY`,
   and reachable under TemperPaw via the ARN-167 header bypass.

The endpoint's own doc comment claims "no filesystem or network access" — false.

## Decision

Two independent layers. Layer B removes the host-compromise primitive and is
sufficient on its own to close the RCE/file-read even in fully-open mode; layer A
restores authorization to the endpoint.

### Sub-Decision B: Host ops are a capability of the dispatch context, not a method

Add an explicit capability to `DispatchContext`:

```rust
pub struct DispatchContext<'a> {
    // ...
    /// Whether this dispatch context may perform host-process operations
    /// (local filesystem reads, spawning `cargo`). True only for a runner
    /// whose host process is the developer's own machine (the local stdio
    /// MCP server). The server-hosted REPL sets this false: its host process
    /// is the Temper server, so host ops there are a host-compromise vector.
    pub allow_host_ops: bool,
}
```

`upload_wasm` and `compile_wasm` are the host ops. When `allow_host_ops` is
false, they are rejected before touching the filesystem or spawning a process,
in the same style as the already-blocked governance writes
(`approve_decision`/`deny_decision`/`set_policy`):

```rust
"upload_wasm" | "compile_wasm" if !ctx.allow_host_ops => Err(format!(
    "temper.{method}() is not available in this context. Host operations \
     (local file read, cargo build) run only on the developer's own machine \
     via the local MCP server, never inside the Temper server process."
)),
"upload_wasm" | "compile_wasm" => dispatch_wasm(ctx, method, args).await,
```

Runner settings:
- `temper-sandbox/src/repl.rs` (server REPL): `allow_host_ops: false`.
- `temper-mcp/src/runtime.rs` (local stdio MCP): `allow_host_ops: true`.

**Why this approach**: it fixes the class of problem — "the server-reachable
dispatch context must not perform host-process operations" — generically, rather
than deleting two method names that could be reintroduced. A future host op is
covered by the same gate. The trust boundary is expressed where it actually lives
(the runner that owns the host process), not smuggled into an unrelated field.

### Sub-Decision A: `/api/repl` requires Cedar authorization

`/api/repl` gains an authorization gate before it runs any code. The REPL can
call every non-host `temper.*` method (entity writes, spec submission, app
install), so it is a privileged capability and must be authorized, not open to
any caller who reaches the port. The gate reuses the server's existing Cedar
enforcement (`authorize_with_context`) against a dedicated `execute_repl` action
on a `Sandbox` resource, recording a governance decision on denial exactly like
the other gated endpoints.

**Why this approach**: it makes the REPL a governed capability using the same
Cedar path as spec submission and policy management, so denials are visible and
approvable through the existing Observe flow. It does not attempt to fix the
global self-asserted-header trust — that is ARN-167 and lands separately; once it
does, this same gate runs against a resolved principal with no code change here.

## Consequences

### Positive
- Arbitrary host file read and RCE via `/api/repl` are removed outright — the
  server-reachable dispatch context can no longer touch the host filesystem or
  spawn processes, in any deployment mode.
- The REPL becomes a Cedar-governed capability; unauthorized use is denied and
  recorded rather than silently executed.
- The host-op trust boundary is now explicit and reusable for any future method.

### Negative
- `upload_wasm`/`compile_wasm` no longer work through `/api/repl`. This is
  intended: those ops only ever made sense against the developer's local
  filesystem, which the server path never had. They remain available through the
  local stdio MCP server, which is their correct home.

### Risks
- If a legitimate flow somewhere relied on `/api/repl` performing host ops, it
  breaks. Investigation found no such caller: both wasm host ops are documented
  as developer/CLI operations and the only production consumer that sets
  `allow_host_ops: true` is the local stdio MCP. Mitigation: the rejection
  message names the correct path (local MCP).

### DST Compliance
- `temper-server` is simulation-visible. The new `allow_host_ops` field is a
  plain `bool` carried on an existing struct; no new time, randomness, threads,
  or I/O are introduced on the sim path. The Cedar gate uses the existing
  `authorize_with_context` + `sim_now`/`sim_uuid` denial-recording path. No new
  `// determinism-ok` annotations required.

## Non-Goals

- Fixing the global self-asserted `x-temper-*` header trust (ARN-167).
- Sandboxing the Monty interpreter further, or restricting the non-host
  `temper.*` methods the REPL may call beyond the authorization gate.
- Changing how `temper-mcp` resolves the wasm SDK path.

## Alternatives Considered

1. **Delete `upload_wasm`/`compile_wasm` from `dispatch_temper_method` entirely.**
   Rejected: it removes a capability the local MCP developer flow legitimately
   uses, and CLAUDE.md forbids dropping working capabilities. The capability is
   not the problem; running it in the server host process is.
2. **Gate host ops on `binary_path.is_some()`.** Rejected: `binary_path` is a
   convenience for SDK resolution, not a trust signal — both current runners pass
   `None`, so this would either block the legitimate MCP path or fail open. Trust
   must be an explicit, named capability.
3. **Auth gate alone (layer A only).** Rejected: it leaves the host-compromise
   primitive intact wherever the REPL is reachable (open standalone kernel, or any
   future auth regression). Removing the primitive is the durable fix.
4. **Make `/api/repl` a dev-only, feature-gated endpoint.** Considered. It would
   remove the endpoint from production entirely, but the REPL is a legitimate
   agent capability (Code Mode) that TemperPaw uses in production; gating it off
   would remove a working capability. Authorization + host-op isolation preserves
   the capability while closing the hole.

## Rollback Policy

Both layers are additive and independently reversible. Reverting Sub-Decision B
restores host ops to the server dispatch (re-opening the primitive); reverting
Sub-Decision A removes the authorization gate. Neither changes on-disk state or
spec formats, so rollback is a code revert with no migration.
