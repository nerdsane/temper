# Temper Skill

Spin up persistent, verified stateful apps with Temper. Any agent, any use case.

## What Temper Is

A Rust state machine backend. You define entities with states, actions, guards, and effects in IOA TOML. Temper gives you:

- **OData API** — CRUD, bound actions (state transitions), SSE events
- **Postgres persistence** — events, trajectories, entity state
- **Verification cascade** — L0-L3 model checking catches spec bugs before runtime
- **Multi-tenant** — one server, many apps, isolated by `X-Tenant-Id`
- **Webhook dispatch** — Temper POSTs to your URL on state transitions

## When to Use This

- You need persistent state that survives restarts (not just a JSON file)
- You have a workflow with defined states and transitions (proposals, content pipeline, task queue, inventory)
- You want verified transitions — illegal moves are rejected, not silently corrupted
- You want a UI that reflects live state and lets humans interact
- Multiple agents need to share state

## Quick Start

### 1. Write a Spec

Create `apps/{your-app}/specs/{entity}.ioa.toml`:

```toml
[entity]
name = "Task"
initial_status = "Open"

[vars]
is_assigned = { type = "bool", init = false }

[[actions]]
name = "Assign"
from = ["Open"]
to = "InProgress"
effects = [{ set = { var = "is_assigned", value = true } }]

[[actions]]
name = "Complete"
from = ["InProgress"]
to = "Done"
guards = [{ is_true = "is_assigned" }]

[[actions]]
name = "Reopen"
from = ["Done"]
to = "Open"
effects = [{ set = { var = "is_assigned", value = false } }]
```

### 2. Start the Server

```bash
cd ~/workspace/Development/temper
./target/release/temper serve --app my-app=apps/my-app/specs --port 3001
```

### 3. Create an Entity

```bash
curl -X POST http://localhost:3001/tdata/Tasks \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: my-app" \
  -d '{"entity_id": "task-001"}'
```

### 4. Fire an Action

```bash
curl -X POST "http://localhost:3001/tdata/Tasks('task-001')/Temper.Assign" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: my-app" \
  -d '{}'
```

Illegal transitions return 409. The state machine enforces correctness.

### 5. Read State

```bash
curl http://localhost:3001/tdata/Tasks \
  -H "X-Tenant-Id: my-app"
```

## IOA Spec Reference

### Entity

```toml
[entity]
name = "MyEntity"
initial_status = "InitialState"
```

### Variables (optional)

```toml
[vars]
my_flag = { type = "bool", init = false }
my_counter = { type = "counter", init = 0 }
```

### Actions

```toml
[[actions]]
name = "ActionName"
from = ["State1", "State2"]   # states this action can fire from
to = "TargetState"             # state after action
guards = [{ is_true = "my_flag" }]  # optional: conditions that must hold
effects = [                    # optional: state variable mutations
  { set = { var = "my_flag", value = true } },
  { increment = { var = "my_counter" } }
]
```

### Guard Types

- `{ is_true = "var_name" }` — bool must be true
- `{ is_false = "var_name" }` — bool must be false
- `{ gt = { var = "counter_name", value = 0 } }` — counter comparison

### Effect Types

- `{ set = { var = "bool_var", value = true } }` — set bool
- `{ increment = { var = "counter_var" } }` — increment counter
- `{ decrement = { var = "counter_var" } }` — decrement counter

### Terminal States

States with no outgoing actions are terminal. Entities in terminal states can't transition further. Design intentionally — don't accidentally trap entities.

### Compound Guards (AND)

Multiple guards in the same action are AND'd:

```toml
guards = [
  { is_true = "reviewed" },
  { is_true = "tested" }
]
```

## OData API

**Base URL:** `http://localhost:{port}/tdata`

| Method | Path | Description |
|--------|------|-------------|
| GET | `/{EntitySet}` | List all entities |
| GET | `/{EntitySet}('{id}')` | Get one entity |
| POST | `/{EntitySet}` | Create entity |
| POST | `/{EntitySet}('{id}')/Temper.{Action}` | Fire action |
| PATCH | `/{EntitySet}('{id}')` | Update fields (not state) |
| GET | `/{EntitySet}('{id}')/events` | Event history |

**Headers:** Always include `X-Tenant-Id: {your-tenant}`.

**Action syntax:** The `Temper.` prefix is required — it's OData bound action syntax.

## Building a UI

Temper UIs are single-file HTML served by a lightweight proxy. Any shape — dashboard, form, map, timeline, kanban.

### Design System

**Always read `apps/shared/design-system.md` before generating any UI.**

It defines tokens (colors, spacing, typography, radius, motion), layout primitives, and component atoms. Every Temper UI uses the same system so they feel cohesive regardless of what they display.

Key rules:
- Inter for prose, JetBrains Mono for data
- Zinc color scale, semantic colors only for state
- `min()`/`clamp()`/`auto-fit` for responsive — no media queries for basic layout
- Borders over shadows
- Labels are quiet (11px, uppercase, muted)

### Proxy Pattern

The UI needs a proxy to serve HTML and forward API calls to Temper. Minimal `serve.py`:

```python
#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper."""
import http.server, urllib.request, json, os

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT = int(os.environ.get("PORT", "8080"))
HTML = os.path.join(os.path.dirname(__file__), "index.html")

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/tdata"):
            self._proxy("GET")
        else:
            self._serve_html()

    def do_POST(self):
        self._proxy("POST")

    def do_PATCH(self):
        self._proxy("PATCH")

    def _serve_html(self):
        with open(HTML, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(data)

    def _proxy(self, method):
        url = f"{TEMPER}{self.path}"
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None
        headers = {"Content-Type": "application/json"}
        tenant = self.headers.get("X-Tenant-Id")
        if tenant:
            headers["X-Tenant-Id"] = tenant
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req) as resp:
                data = resp.read()
                self.send_response(resp.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(data)
        except urllib.error.HTTPError as e:
            self.send_response(e.code)
            self.end_headers()
            self.wfile.write(e.read())

    def log_message(self, fmt, *args):
        pass  # quiet

if __name__ == "__main__":
    print(f"Serving on :{PORT} → {TEMPER}")
    http.server.HTTPServer(("", PORT), Handler).serve_forever()
```

### Exposing via Tunnel

```bash
# Cloudflare tunnel (ephemeral, free)
nohup cloudflared tunnel --url http://localhost:8080 > /tmp/tunnel.log 2>&1 &
grep -o 'https://[^ ]*trycloudflare.com' /tmp/tunnel.log
```

## Webhooks

Temper can POST to a URL when state transitions happen. Configure in your spec directory:

```toml
# apps/{your-app}/specs/webhooks.toml
[[webhooks]]
url = "http://127.0.0.1:18789/hooks/wake"
actions = ["Select", "Approve", "Complete"]  # optional filter
entity_types = ["Proposal"]                   # optional filter
```

This lets agents react to human actions in real time — no polling.

## Multi-Tenant

One Temper server hosts many apps. Each app is a tenant with isolated data.

```bash
# Start with multiple apps
./target/release/temper serve \
  --app haku-ops=apps/haku-ops/specs \
  --app calcifer-content=apps/calcifer-content/specs \
  --port 3001
```

Every API request includes `X-Tenant-Id` to route to the right app.

## Verification

Temper verifies specs at load time (L0-L3):

- **L0:** Syntax — TOML parses correctly
- **L1:** Model checking — reachability, deadlocks, guard consistency
- **L2:** Property testing — random action sequences, invariant checking
- **L3:** Bounded model checking (Z3) — exhaustive state space exploration

If verification fails, the server won't start. Fix the spec.

Common spec bugs caught by verification:
- Actions from terminal states (nothing can leave a terminal state)
- Guards referencing undefined variables
- Unreachable states (no action path leads there)
- Effect syntax errors (`set_bool` → `set`)

## Example Use Cases

| Agent | App | Entities |
|-------|-----|----------|
| Haku | haku-ops | Proposals, Findings, CCSessions, Deployments |
| Calcifer | calcifer-content | Posts (Draft→Reviewed→Published), Campaigns |
| Chihiro | chihiro-tasks | Tasks (Open→InProgress→Done→Archived) |
| Any | generic-queue | Items (Pending→Processing→Complete→Failed) |

## File Structure

```
apps/{your-app}/
├── specs/
│   ├── entity1.ioa.toml
│   ├── entity2.ioa.toml
│   └── webhooks.toml       # optional
├── index.html               # UI (any shape)
├── serve.py                 # proxy
├── seed.sh                  # optional: bootstrap data
└── README.md
```

## Environment

```bash
# Required for building Temper
. "$HOME/.cargo/env"
export Z3_SYS_Z3_HEADER="/opt/homebrew/include/z3.h"
export BINDGEN_EXTRA_CLANG_ARGS="-I/opt/homebrew/include"
export LIBRARY_PATH="/opt/homebrew/lib"
export PATH="/opt/homebrew/opt/postgresql@17/bin:$PATH"
export DATABASE_URL="postgres://temper:temper_dev@localhost/haku_ops"
```

## Key Constraints

- **OData bound actions require `Temper.` prefix:** `POST /EntitySet('id')/Temper.ActionName`
- **Illegal transitions return 409**, not silent corruption
- **Fields bag (via PATCH) doesn't change state** — it's metadata, not transitions
- **Entity state is derived from events** — the event log is the source of truth
- **Seed scripts re-run after restart** if in-memory state is lost (hydration from Postgres fixes this)
