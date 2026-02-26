# Temper Agent — Governed MCP Operations

**Use this skill when Claude Code needs to operate through the Temper MCP tool (`mcp__temper__execute` or `mcp__temper__search`).** This skill teaches you the Python sandbox API, spec format, and governance flow so you can call the MCP tools correctly on the first try.

You are operating inside a governed sandbox. You cannot import libraries, access the filesystem, or make network calls directly. All operations go through the `temper` object methods, which are `await`-based. The server enforces Cedar authorization — actions may be denied, requiring human approval before you can proceed.

## Quick Start

### 1. Start the server

```python
result = await temper.start_server()
return result
```

Returns: `{"port": N, "storage": "turso", "observe_url": "http://localhost:3001", "apps": [...], "status": "started"}`

If already running, returns `{"port": N, "status": "already_running"}`.

### 2. Submit specs (IOA + CSDL)

```python
ioa = """[automaton]
name = "WeatherQuery"
states = ["Idle", "Fetching", "Ready"]
initial = "Idle"

[[action]]
name = "FetchWeather"
kind = "input"
from = ["Idle"]
to = "Fetching"
params = ["city"]
effect = "trigger fetch_weather"

[[action]]
name = "FetchSucceeded"
kind = "input"
from = ["Fetching"]
to = "Ready"
params = ["temperature", "conditions"]

[[action]]
name = "FetchFailed"
kind = "input"
from = ["Fetching"]
to = "Idle"

[[action]]
name = "Reset"
kind = "input"
from = ["Ready"]
to = "Idle"

[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://wttr.in/{city}?format=j1"
method = "GET"
"""

csdl = """<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Weather" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="WeatherQuery">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
        <Property Name="city" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="WeatherQueries" EntityType="Weather.WeatherQuery"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"""

result = await temper.submit_specs("default", {
    "WeatherQuery.ioa.toml": ioa,
    "model.csdl.xml": csdl
})
return result
```

### 3. Create an entity and invoke an action

```python
created = await temper.create("default", "WeatherQueries", {"id": "q1", "city": "London"})
result = await temper.action("default", "WeatherQueries", "q1", "FetchWeather", {"city": "London"})
return result
```

### 4. Handle authorization denials (CRITICAL)

When Cedar denies an action, you get an error containing `AuthorizationDenied` and a decision ID like `PD-<uuid>`. You MUST wait for human approval:

```python
# After getting AuthorizationDenied with decision ID PD-abc123:
decision = await temper.poll_decision("default", "PD-abc123")
return decision
# Returns: {"status": "Approved", ...} or {"status": "Denied", ...}
```

Then retry the original action if approved.

**You CANNOT self-approve.** Calling `approve_decision`, `deny_decision`, or `set_policy` will return an error. A human must approve via the Observe UI (http://localhost:3001) or `temper decide` CLI.

---

## Method Reference

### Lifecycle

| Method | Signature | Returns |
|--------|-----------|---------|
| `start_server` | `await temper.start_server()` | `{"port", "storage", "observe_url", "apps", "status"}` |

### Entity Operations

All take `(tenant, entity_set, ...)`. The `entity_set` is the **plural collection name** (e.g., `"WeatherQueries"`, `"Orders"`, `"Bugs"`) — NOT the entity type name.

| Method | Signature | Returns |
|--------|-----------|---------|
| `list` | `await temper.list(tenant, entity_set)` | Array of entities |
| `get` | `await temper.get(tenant, entity_set, entity_id)` | Single entity |
| `create` | `await temper.create(tenant, entity_set, fields)` | Created entity |
| `action` | `await temper.action(tenant, entity_set, entity_id, action_name, params)` | Action result |
| `patch` | `await temper.patch(tenant, entity_set, entity_id, fields)` | Updated entity |

### Spec Operations

| Method | Signature | Returns |
|--------|-----------|---------|
| `submit_specs` | `await temper.submit_specs(tenant, {"file.ioa.toml": content, "model.csdl.xml": content})` | Verification results |
| `show_spec` | `await temper.show_spec(tenant, entity_type)` | Full parsed spec JSON |
| `get_policies` | `await temper.get_policies(tenant)` | Cedar policies |
| `upload_wasm` | `await temper.upload_wasm(tenant, module_name, wasm_path)` | Upload status |
| `compile_wasm` | `await temper.compile_wasm(tenant, module_name, rust_source)` | Compile + upload |

### Governance (Read-Only)

| Method | Signature | Returns |
|--------|-----------|---------|
| `get_decisions` | `await temper.get_decisions(tenant)` | Array of pending decisions |
| `poll_decision` | `await temper.poll_decision(tenant, decision_id)` | Blocks until resolved (30s max) |

### Search Tool (No Server Required)

Use `mcp__temper__search` instead of `mcp__temper__execute` for read-only spec queries:

| Method | Signature | Returns |
|--------|-----------|---------|
| `tenants` | `await spec.tenants()` | List of tenant names |
| `entities` | `await spec.entities(tenant)` | List of entity types |
| `describe` | `await spec.describe(tenant, entity_type)` | Full entity description |
| `actions` | `await spec.actions(tenant, entity_type)` | List of actions with details |
| `actions_from` | `await spec.actions_from(tenant, entity_type, state)` | Actions available from a state |
| `raw` | `await spec.raw(tenant, entity_type)` | Raw IOA spec text |

---

## IOA Spec Format

**CRITICAL: Use `[automaton]` table header (NOT `automaton WeatherQuery` bare text).** Use `initial` (NOT `initial_state`).

```toml
[automaton]
name = "EntityName"
states = ["State1", "State2", "State3"]
initial = "State1"

# Optional state variables
[[state]]
name = "counter_var"
type = "counter"        # "counter" | "bool"
initial = "0"

# Actions (state transitions)
[[action]]
name = "DoSomething"
kind = "input"          # "input" | "internal" | "output"
from = ["State1"]       # states this can fire from
to = "State2"           # target state
guard = "counter_var > 0"  # optional precondition
effect = "trigger some_integration"  # optional
params = ["Param1"]     # optional parameters
hint = "Description."   # optional

# Safety invariants
[[invariant]]
name = "FinalIsFinal"
when = ["State3"]
assert = "no_further_transitions"

# Liveness properties
[[liveness]]
name = "EventuallyDone"
from = ["State1"]
reaches = ["State3"]

# WASM integrations for external API calls
[[integration]]
name = "some_integration"
trigger = "some_integration"   # matches the effect trigger name
type = "wasm"
module = "http_fetch"          # built-in module for HTTP calls
on_success = "CallbackOk"     # action to invoke on success
on_failure = "CallbackFail"   # action to invoke on failure
url = "https://api.example.com/endpoint"  # extra config for the module
method = "GET"                 # extra config for the module
```

### Built-in WASM Module: `http_fetch`

The `http_fetch` module makes HTTP requests. Configure it via integration config keys:

| Key | Required | Description |
|-----|----------|-------------|
| `url` | Yes | URL template (supports `{param}` substitution from action params) |
| `method` | Yes | HTTP method: `GET`, `POST`, `PUT`, `DELETE` |
| `body` | No | Request body template (for POST/PUT) |

Example — weather API:
```toml
[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://wttr.in/{city}?format=j1"
method = "GET"
```

The callback action receives `{"status_code": "200", "body": "...response..."}` as params.

---

## Governance Flow

```
You call action → Cedar evaluates policy → DENIED (403)
  → Error contains "AuthorizationDenied" + decision ID (PD-xxx)
  → You call poll_decision(tenant, "PD-xxx") to wait
  → Human approves via Observe UI or `temper decide` CLI
  → poll_decision returns with status "Approved"
  → You retry the original action → SUCCESS
```

**Rules:**
- NEVER try to approve your own decisions
- NEVER call `approve_decision`, `deny_decision`, or `set_policy` — they are blocked
- ALWAYS use `poll_decision` to wait after a denial
- Tell the user what's pending so they can approve it

---

## Common Patterns

### Full weather query flow

```python
# Start server
await temper.start_server()

# Submit specs
await temper.submit_specs("default", {
    "WeatherQuery.ioa.toml": ioa_spec,
    "model.csdl.xml": csdl
})

# Create entity
await temper.create("default", "WeatherQueries", {"id": "q1", "city": "London"})

# Trigger weather fetch (may be denied by Cedar — handle it!)
try:
    result = await temper.action("default", "WeatherQueries", "q1", "FetchWeather", {"city": "London"})
    return result
except Exception as e:
    error = str(e)
    if "AuthorizationDenied" in error:
        # Extract PD-xxx from the error message
        # Wait for human approval
        return error  # Show the user what needs approval
    return error
```

### List and inspect entities

```python
entities = await temper.list("default", "WeatherQueries")
return entities
```

### Check available actions from current state

Use the search tool (no server needed):
```python
actions = await spec.actions_from("default", "WeatherQuery", "Idle")
return actions
```

---

## CSDL Format (Minimal)

For simple entities, a minimal CSDL works:

```xml
<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="MyApp" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EntityName">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
        <!-- Add domain properties here -->
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="EntityNames" EntityType="MyApp.EntityName"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
```

**Key rules:**
- Every entity MUST have `id` (key) and `state` properties
- EntitySet name is typically the plural of EntityType name
- Namespace in CSDL doesn't need to match IOA — it's for OData routing
- The EntitySet name is what you pass to `temper.list()`, `temper.create()`, etc.

---

## Errors and What They Mean

| Error | Meaning | What to Do |
|-------|---------|------------|
| `HTTP 400 Bad Request: Failed to parse IOA spec` | Spec syntax error | Check `[automaton]` header, `initial` field, state names |
| `HTTP 409 Conflict` | Invalid state transition | Check `from` states — action can't fire from current state |
| `HTTP 423 Locked` | Entity not verified | Wait for spec verification to complete |
| `AuthorizationDenied` | Cedar policy denied the action | Use `poll_decision` and wait for human approval |
| `not available to agents` | Tried to self-approve | Stop — only humans can approve/deny/set policies |
| `Server not started` | Called method before start_server | Call `await temper.start_server()` first |

## Sandbox Constraints

- **No imports** — `import os`, `import requests`, etc. are blocked
- **No filesystem** — `open()`, `os.path`, etc. are blocked
- **No network** — `urllib`, `socket`, etc. are blocked
- **2 second timeout** — code must complete within 2 seconds
- **64 MB memory** — stay within bounds
- Only `temper.*` and `spec.*` methods are available for I/O
