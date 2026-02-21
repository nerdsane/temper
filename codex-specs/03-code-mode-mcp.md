# Codex Spec: Code Mode MCP Server

## Goal
Add `temper mcp` subcommand that starts an MCP server (stdio transport) exposing Temper via Code Mode — two tools: `search()` and `execute()`.

## Context
- Cloudflare Code Mode pattern: https://blog.cloudflare.com/code-mode-mcp/
- Pydantic Monty (Rust Python sandbox): https://github.com/pydantic/monty — use as the code execution sandbox
- Temper IOA specs: TOML files defining entity state machines
- Temper OData API: REST at `/tdata`
- MCP spec: https://modelcontextprotocol.io

## Requirements

### New crate: `crates/temper-mcp/`
- Add to workspace `Cargo.toml`
- Dependencies: `pydantic-monty` (Rust crate), `mcp-server` or hand-roll stdio JSON-RPC

### CLI subcommand
```
temper mcp --app haku-ops=apps/haku-ops/specs --port 3001
```
Starts an MCP server on stdio. The `--port` tells it where the Temper HTTP server is running (for `execute()`). The `--app` loads specs for `search()`.

### Tool: `search(code: str) -> str`
- Purpose: explore loaded IOA specs programmatically
- The Python code receives a `spec` object containing all loaded IOA specs as dicts:
  ```python
  spec = {
    "haku-ops": {
      "entities": {
        "Proposal": {
          "states": ["Seed", "Planned", "Approved", ...],
          "initial": "Seed",
          "actions": [
            {"name": "WritePlan", "from": ["Seed"], "to": "Planned", "guards": [...], "effects": [...]},
            ...
          ],
          "vars": {"is_selected": {"type": "bool", "init": false}, ...}
        },
        ...
      }
    }
  }
  ```
- Monty executes the code with `spec` as input, returns the result as JSON string
- Example: `search("return [a['name'] for e in spec['haku-ops']['entities'].values() for a in e['actions']]")`

### Tool: `execute(code: str) -> str`
- Purpose: run operations against the live Temper API
- The Python code gets a `temper` object with external functions:
  - `temper.list(tenant, entity_type)` → GET `/tdata/{EntitySet}` with `X-Tenant-Id`
  - `temper.get(tenant, entity_type, entity_id)` → GET `/tdata/{EntitySet}('{id}')`
  - `temper.create(tenant, entity_type, fields)` → POST `/tdata/{EntitySet}`
  - `temper.action(tenant, entity_type, entity_id, action_name, body)` → POST `/tdata/{EntitySet}('{id}')/Temper.{Action}`
  - `temper.patch(tenant, entity_type, entity_id, fields)` → PATCH `/tdata/{EntitySet}('{id}')`
- These are Monty external functions — they make HTTP calls to the Temper server
- Example: `execute("tasks = await temper.list('haku-ops', 'Proposals'); return [t for t in tasks if t['status'] == 'Seed']")`

### MCP Protocol
Implement JSON-RPC over stdio per MCP spec:
- `initialize` → return server info + capabilities (tools)
- `tools/list` → return `search` and `execute` tool definitions
- `tools/call` → dispatch to search or execute
- Proper error handling: Monty execution errors return as tool error results, not crashes

### Security
- Monty sandbox: no filesystem, no env vars, no raw network access
- Only the defined external functions (`temper.*`) can make network calls
- External functions only talk to `localhost:{port}` — the Temper server
- Type checking via Monty's ty integration (optional, nice-to-have)

### Tests
- Test: MCP initialize handshake
- Test: search returns filtered spec data
- Test: execute creates entity and reads it back
- Test: execute with invalid action returns Temper's 409 error cleanly
- Test: sandbox blocks filesystem access
- Test: compound operation (create + action + query in single execute)

### Do NOT
- Add web/HTTP transport (stdio only for now)
- Modify existing crates
- Expose raw HTTP — everything goes through the `temper.*` external functions
