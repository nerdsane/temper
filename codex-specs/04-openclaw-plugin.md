# Codex Spec: OpenClaw Temper Plugin

## Goal
Build an OpenClaw plugin that gives agents native Temper integration: a `temper` agent tool for querying/acting on entities, and a background SSE service that injects Temper state changes into agent sessions.

## Context
- OpenClaw plugin docs: read `/opt/homebrew/lib/node_modules/openclaw/docs/tools/plugin.md` and `/opt/homebrew/lib/node_modules/openclaw/docs/plugins/agent-tools.md`
- OpenClaw plugin SDK types: `/opt/homebrew/lib/node_modules/openclaw/dist/plugin-sdk/plugins/types.d.ts`
- Reference plugin: `/opt/homebrew/lib/node_modules/openclaw/extensions/voice-call/index.ts`
- Temper OData API: `http://localhost:3001/tdata/{EntitySet}` with `X-Tenant-Id` header
- Temper SSE: `http://localhost:3001/tdata/$events` (Server-Sent Events, broadcasts entity state changes as JSON)
- OpenClaw hooks endpoint: `http://127.0.0.1:18789/hooks/wake` (injects system event into agent session)

## Plugin Directory: `plugins/openclaw-temper/`

### Files to create

#### `plugins/openclaw-temper/openclaw.plugin.json`
```json
{
  "id": "temper",
  "name": "Temper",
  "description": "Temper state machine integration — real-time entity subscriptions and agent tools",
  "configSchema": {
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "url": { "type": "string", "description": "Temper server URL" },
      "apps": {
        "type": "object",
        "additionalProperties": {
          "type": "object",
          "properties": {
            "agent": { "type": "string", "description": "Agent ID to route events to" },
            "subscribe": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Entity types to subscribe to"
            }
          },
          "required": ["agent"]
        }
      },
      "hooksToken": { "type": "string", "description": "OpenClaw hooks bearer token" },
      "hooksPort": { "type": "integer", "description": "OpenClaw gateway port (default 18789)" }
    },
    "required": ["url", "apps"]
  },
  "uiHints": {
    "url": { "label": "Temper URL", "placeholder": "http://localhost:3001" },
    "hooksToken": { "label": "Hooks Token", "sensitive": true },
    "hooksPort": { "label": "Gateway Port", "placeholder": "18789" }
  }
}
```

#### `plugins/openclaw-temper/package.json`
```json
{
  "name": "@temper/openclaw-plugin",
  "version": "0.1.0",
  "openclaw": {
    "extensions": ["./index.ts"]
  },
  "dependencies": {}
}
```

#### `plugins/openclaw-temper/index.ts`

This is the main plugin file. It must:

1. **Register a `temper` agent tool** with these operations:
   - `list(app, entityType)` — GET `/tdata/{entityType}` with `X-Tenant-Id: {app}`
   - `get(app, entityType, entityId)` — GET `/tdata/{entityType}('{entityId}')` with tenant header
   - `create(app, entityType, body)` — POST `/tdata/{entityType}` with body + tenant header
   - `action(app, entityType, entityId, actionName, body?)` — POST `/tdata/{entityType}('{entityId}')/Temper.{actionName}` with body + tenant header
   - `patch(app, entityType, entityId, body)` — PATCH `/tdata/{entityType}('{entityId}')` with body + tenant header
   - Return Temper's response directly as text content. For errors (4xx/5xx), return the error body with `isError: true`.

2. **Register a background service** (`registerService`) that:
   - On `start`: connects to Temper SSE at `{url}/tdata/$events` using `EventSource` or plain `fetch` with streaming
   - For each SSE event: parse the JSON, check if `entity_type` matches any app's `subscribe` list
   - If matched: POST to `http://127.0.0.1:{hooksPort}/hooks/wake` with:
     ```json
     {
       "text": "Temper: {action} on {entity_type} {entity_id} ({from_status} → {to_status}) [app: {app_name}]",
       "mode": "now"
     }
     ```
     Headers: `Authorization: Bearer {hooksToken}`, `Content-Type: application/json`
   - Auto-reconnect on SSE disconnect with exponential backoff (1s, 2s, 4s, 8s, max 30s)
   - Log connection status via `api.logger`
   - On `stop`: close the SSE connection

3. **Use `api.on("gateway_start")`** to log that Temper plugin is ready

4. **Multi-agent routing**: When an SSE event matches an app, use the app's `agent` field to determine which agent to wake. Pass `agentId` in the wake request if the `/hooks/agent` endpoint is used instead of `/hooks/wake`.

### Tool Parameters Schema (for registerTool)

```typescript
{
  name: "temper",
  description: "Query and act on Temper state machine entities. Operations: list, get, create, action, patch.",
  parameters: {
    type: "object",
    properties: {
      operation: {
        type: "string",
        enum: ["list", "get", "create", "action", "patch"],
        description: "Operation to perform"
      },
      app: {
        type: "string",
        description: "Tenant/app name (e.g. 'haku-ops')"
      },
      entityType: {
        type: "string",
        description: "Entity set name (e.g. 'Proposals', 'Tasks')"
      },
      entityId: {
        type: "string",
        description: "Entity ID (for get/action/patch)"
      },
      actionName: {
        type: "string",
        description: "Action name (for action operation, e.g. 'Approve')"
      },
      body: {
        type: "object",
        description: "Request body (for create/action/patch)"
      }
    },
    required: ["operation", "app", "entityType"]
  }
}
```

### Important implementation notes

- Use `fetch` (Node.js native) for HTTP requests to Temper, NOT axios or other deps
- For SSE, use Node.js native `fetch` with `response.body` readable stream, or the `eventsource` npm package. If using native fetch, parse the `text/event-stream` format manually (split on `\n\n`, extract `data:` lines)
- The plugin runs in-process with OpenClaw gateway — no sandboxing needed
- Import types from `"openclaw/plugin-sdk"` 
- Config is accessed via `api.pluginConfig`
- The tool's `execute` function receives `(toolCallId, params, context?)` where context has `agentId`, `sessionKey` etc.
- Tool must return `{ content: [{ type: "text", text: "..." }] }` format
- For errors, return `{ content: [{ type: "text", text: "..." }], isError: true }`

### Do NOT
- Add unnecessary npm dependencies (use Node.js native fetch)
- Modify any OpenClaw source code
- Modify any Temper Rust source code
- Create test files (no test infrastructure for OpenClaw plugins in this repo)
