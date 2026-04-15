# ADR-0006: Spec-Aware Agent Interface for MCP Code Mode

## Status
Accepted

## Context
ADR-0004 added Cedar authorization and ADR-0005 added the policy & audit layer with approval flows. But the MCP REPL — the primary interface for agents operating Temper — had no awareness of loaded specs, no developer tools for submitting specs or managing policies, and no governance methods for the deny→approve→retry loop.

An agent connected via MCP could call `search` and `execute` but couldn't:
- Discover what entity types and actions are available without raw JSON inspection
- Submit new specs or manage Cedar policies through the REPL
- React to authorization denials programmatically (no structured errors, no decision IDs)

The gap between the server's governance capabilities and the agent's interface made the full Agent OS loop impossible through MCP alone.

## Decision

### Spec Injection via TemperSpec Dataclass

Inject loaded specs into the `search` tool as a frozen Monty `Dataclass` (type ID 10) named `TemperSpec`. The user's Python code receives it as the `spec` parameter with six methods:

| Method | Returns | Purpose |
|--------|---------|---------|
| `tenants()` | `[str]` | List loaded tenants |
| `entities(tenant)` | `[str]` | List entity types |
| `describe(tenant, entity)` | object | Full entity spec (states, actions, vars) |
| `actions(tenant, entity)` | `[action]` | All actions with params, guards, effects |
| `actions_from(tenant, entity, state)` | `[action]` | Actions available from a specific state |
| `raw(tenant, entity)` | JSON | Complete spec structure |

Methods dispatch to Rust via `dispatch_spec_method()` — no JSON conversion overhead. Frozen dataclass prevents modification.

### Dynamic Tool Descriptions

`tool_definitions()` now takes `&RuntimeContext` and appends a live summary of loaded specs to both `search` and `execute` tool descriptions. Example: `"Loaded: ecommerce (Order, Payment, Shipment); internal (Config)"`.

This means the agent always knows what's available without a separate introspection call, and descriptions update as specs are submitted.

### Eight New Execute Methods

**Developer methods** (spec and policy management):

| Method | Purpose |
|--------|---------|
| `show_spec(tenant, entity)` | Read spec from in-memory cache (no server call) |
| `submit_specs(tenant, {filename: content})` | POST to `/api/specs/load-inline` with inline spec text |
| `set_policy(tenant, cedar_text)` | PUT Cedar policy to `/api/tenants/{t}/policies` |
| `get_policies(tenant)` | GET current Cedar policies |

**Governance methods** (decision lifecycle):

| Method | Purpose |
|--------|---------|
| `get_decisions(tenant, status?)` | GET decisions, optionally filtered by status |
| `approve_decision(tenant, id, scope)` | POST approval with scope + `decided_by: "mcp-agent"` |
| `deny_decision(tenant, id)` | POST denial |
| `poll_decision(tenant, id)` | Loop polling every 1s, 30s timeout, return final status |

### Agent Identity Headers

When `McpConfig.principal_id` is set (via CLI `--principal-id`), all HTTP requests to the Temper server include:
- `X-Temper-Principal-Kind: agent`
- `X-Temper-Principal-Id: {principal_id}`

Both `temper_request()` (JSON body) and `temper_request_text()` (plain text body) conditionally inject these headers. This enables Cedar policies to distinguish between agents.

### Structured 403 Error Handling

When a request returns HTTP 403 with `AuthorizationDenied` error code, `format_authz_denied()` in the sandbox:
1. Parses the response body for the `PD-{uuid}` decision ID
2. Formats a rich error message with the decision ID and guidance
3. Directs the agent to `get_decisions()` or `approve_decision()`

This enables the full programmatic governance loop: agent denied → extract decision ID → approve → retry.

## Consequences

### Positive
- **Full Agent OS loop through MCP**: An agent can discover specs, submit new ones, manage policies, and handle denials — all without leaving the REPL
- **Self-describing interface**: Dynamic tool descriptions eliminate the "what's loaded?" bootstrapping problem
- **Programmatic governance**: Structured 403 errors with decision IDs make deny→approve→retry automatable
- **Backward compatible**: No principal_id configured → no identity headers → permissive mode unchanged

### Negative
- **Method surface area**: 6 spec methods + 8 execute methods is a large API for an LLM to navigate. Mitigated by grouping in tool descriptions and clear naming.
- **poll_decision blocking**: The 30s timeout blocks the agent's execution thread. Mitigated by making the timeout configurable in future.
- **show_spec reads cache**: If specs are modified on the server side outside MCP, `show_spec` returns stale data. Mitigated by the fact that MCP is the primary modification interface.
