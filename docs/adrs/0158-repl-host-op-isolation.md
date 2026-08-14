# ADR-0158: REPL host-operation isolation

**Status:** Accepted
**Date:** 2026-08-14
**Issue:** ARN-166 (kernel `/api/repl` unauthenticated → arbitrary host file-read + RCE)

## Context

`temper.*` methods are dispatched from Python REPL code through
`temper_sandbox::dispatch::dispatch_temper_method`. Two of those methods,
`upload_wasm` and `compile_wasm`, are **host-process operations**: `upload_wasm`
reads a WASM file from the local filesystem, and `compile_wasm` writes a crate
to disk and spawns `cargo build`.

The same dispatch path runs in two very different host processes:

1. **Local stdio MCP server** — runs on the *developer's own machine*. Reading a
   local file and running `cargo` there is exactly what the developer intends.
2. **Server-hosted REPL** (`POST /api/repl`) — runs *inside the Temper server
   process*. There, `upload_wasm` reads the **server's** filesystem and
   `compile_wasm` runs `cargo` **as the server user**.

ARN-170 (merged) already made `/api/repl` require an authenticated credential and
a Cedar `execute_repl` permit. But authentication alone does not close ARN-166:
an *authorized* caller could still invoke `compile_wasm` and achieve arbitrary
file read + code execution as the server user. Authorization answers "may this
principal use the REPL"; it does not answer "may the REPL reach the host."

## Decision

Make host reachability an explicit capability of the dispatch context, not an
ambient property of the method.

- `DispatchContext` gains `allow_host_ops: bool`.
- `dispatch_temper_method` rejects `upload_wasm`/`compile_wasm` **before touching
  the filesystem** when `allow_host_ops` is false, with a message that names
  where those ops belong (the local MCP server).
- The server-hosted REPL sets `allow_host_ops = false` (via `ReplConfig`).
- The local stdio MCP server sets `allow_host_ops = true`.

The gate is the capability flag, not a hardcoded block: with the flag set, the
same two methods proceed normally — verified by a test that exercises both arms.

## Consequences

- The RCE/file-read vector on the server-hosted REPL is closed at the dispatch
  boundary, independent of authorization. Even a fully authorized `execute_repl`
  caller cannot drive host ops in the server process.
- All other `temper.*` methods (entity CRUD, specs, governance, evolution,
  non-host WASM registry reads) remain available in the server REPL — the fix
  removes only the two host-process sinks, not the REPL's usefulness.
- Local developer workflows (`temper.upload_wasm`, `temper.compile_wasm` via the
  MCP server) are unchanged.

## Alternatives considered

- **Rely on the ARN-170 auth gate alone.** Rejected: authorization is not
  isolation; an authorized principal would still get RCE as the server user.
- **Remove `upload_wasm`/`compile_wasm` from dispatch entirely.** Rejected:
  removes a legitimate local developer capability; the two contexts have
  genuinely different trust, so the capability, not the method, is what varies.
- **Sandbox the server-side `cargo` invocation.** Larger surface, still leaves
  local file reads; the capability gate is the smaller, complete fix.
