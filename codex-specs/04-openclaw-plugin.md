# Codex Spec: OpenClaw Temper Plugin

## Goal
Build an OpenClaw plugin that gives agents native Temper integration: real-time state change notifications via SSE and a `temper` tool for querying/acting on entities.

## Context
- OpenClaw plugin docs: https://docs.openclaw.ai/plugins
- OpenClaw repo: https://github.com/openclaw/openclaw
- Temper SSE: `/tdata/$events` (Server-Sent Events, broadcasts state changes)
- Temper OData API: `/tdata/{EntitySet}`, actions via `/tdata/{EntitySet}('{id}')/Temper.{Action}`

## Requirements

### Plugin directory: `plugins/openclaw-temper/`
- TypeScript/Node.js package
- `package.json` with OpenClaw plugin metadata

### Configuration schema
Users add to their `openclaw.json`:
```json
{
  "plugins": {
    "temper": {
      "url": "http://localhost:3001",
      "apps": {
        "haku-ops": {
          "agent": "haku",
          "subscribe": ["Proposal", "Finding"]
        },
        "calcifer-content": {
          "agent": "calcifer",
          "subscribe": ["Post"]
        }
      }
    }
  }
}
```

### SSE Subscriber
- On plugin start, connect to `{url}/tdata/$events` via EventSource
- For each event, match `entity_type` against subscribed types for each app
- Route matched events as system events into the correct agent's session
- System event format: `Temper: {action} on {entity_type} {entity_id} ({from_status} → {to_status}) [app: {app_name}]`
- Auto-reconnect on SSE disconnect (exponential backoff, max 30s)
- Log connection status at startup

### Tool: `temper`
Expose a tool that agents can call:

```typescript
{
  name: "temper",
  description: "Query and act on Temper entities (state machines)",
  parameters: {
    app: "string — tenant/app name",
    operation: "list | get | create | action | patch",
    entity_type: "string — e.g. Proposals, Tasks",
    entity_id: "string? — for get/action/patch",
    action_name: "string? — for action operation",
    body: "object? — for create/action/patch"
  }
}
```

Operations:
- `list` → GET `/tdata/{entity_type}` with `X-Tenant-Id: {app}`
- `get` → GET `/tdata/{entity_type}('{entity_id}')` with tenant header
- `create` → POST `/tdata/{entity_type}` with body + tenant header
- `action` → POST `/tdata/{entity_type}('{entity_id}')/Temper.{action_name}` with body + tenant header
- `patch` → PATCH `/tdata/{entity_type}('{entity_id}')` with body + tenant header

Return Temper's response directly. For 409 (illegal transition), return the error message clearly.

### Multi-agent isolation
- Each app maps to one agent
- Events for `haku-ops` only go to haku's session
- Events for `calcifer-content` only go to calcifer's session
- The `temper` tool respects this: agents can only access their mapped apps (or explicitly shared ones)

### Tests
- Test: SSE connection and reconnect
- Test: event routing to correct agent
- Test: tool operations (list, get, create, action, patch)
- Test: 409 error handling
- Test: multi-agent isolation (events don't cross)

### Research needed
- Read OpenClaw plugin API docs to understand: how to register tools, how to inject system events, how to identify which agent is calling
- This spec may need revision after reading the plugin API — the tool registration and event injection patterns depend on OpenClaw's specific plugin interface

### Do NOT
- Modify Temper source code
- Add polling fallback (SSE only)
- Bundle the Temper server — this plugin connects to an external Temper instance
