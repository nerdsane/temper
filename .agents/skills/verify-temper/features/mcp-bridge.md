# MCP bridge and REPL

## Sub-features
Stdio MCP server, sandboxed Python REPL, `temper.*` API (submit specs, create entities, invoke actions).

## How to get to it (user POV)
Agent clients (Claude Code, Codex) connect over stdio: `cargo run -p temper-cli -- mcp` proxies to a running serve instance.

## Driving it
Start serve first, then the bridge. Through the REPL: `await temper.specs("default")`, create an entity, invoke an action, read it back over OData.

## Gotchas
The bridge proxies - it does not serve. A dead serve behind it turns every call into a transport error that looks like an auth failure.
