# Codex Spec: Temper Browser Client (`temper-client.js`)

## Goal
Build a lightweight JavaScript client library served by the Temper server that any HTML app can include with one `<script>` tag to get real-time entity subscriptions and a simple API for CRUD + actions. This makes every Temper-backed app automatically a shared workspace — human and agent both see changes in real time without custom wiring.

## Context
- Temper OData API: `http://localhost:3001/tdata/{EntitySet}` with `X-Tenant-Id` header
- Temper SSE endpoint: `http://localhost:3001/tdata/$events` (Server-Sent Events, broadcasts entity state changes as JSON)
- Temper server crate: `crates/temper-server/` (axum-based)
- Existing SSE implementation: `crates/temper-server/src/events.rs`
- Existing routes: `crates/temper-server/src/routes.rs`

## What to build

### 1. JavaScript client: `crates/temper-server/static/temper-client.js`

A single vanilla JS file (no build step, no dependencies, no TypeScript, no modules — just a plain `<script>` include). Must work in all modern browsers.

**API surface:**

```javascript
// Create a client instance
const t = new Temper('http://localhost:3001', 'my-tenant');

// === CRUD + Actions (Promise-based) ===

// List entities
const proposals = await t.list('Proposals');
// Returns: array of entity objects

// List with OData filter
const seeds = await t.list('Proposals', { filter: "Status eq 'Seed'" });

// Get single entity
const p = await t.get('Proposals', 'entity-uuid');
// Returns: entity object

// Create entity
const created = await t.create('Proposals', { Title: 'New thing' });
// Returns: created entity object

// Fire action
const result = await t.action('Proposals', 'entity-uuid', 'Approve');
// With body:
const result2 = await t.action('Proposals', 'entity-uuid', 'Approve', { notes: 'lgtm' });
// Returns: updated entity object

// Patch fields
await t.patch('Proposals', 'entity-uuid', { Title: 'Updated' });

// === Real-time subscriptions (SSE) ===

// Subscribe to all events for this tenant
t.on('*', (event) => {
  // event: { entity_type, entity_id, action, from_status, to_status, timestamp, ... }
  console.log(event);
});

// Subscribe to specific entity type
t.on('Proposal', (event) => {
  updateProposalRow(event.entity_id, event);
});

// Unsubscribe
const unsub = t.on('Proposal', handler);
unsub(); // removes this listener

// Connection status
t.onStatus((status) => {
  // status: 'connecting' | 'connected' | 'disconnected' | 'reconnecting'
  updateConnectionIndicator(status);
});

// Manual connect/disconnect (auto-connects on first .on() call)
t.connect();
t.disconnect();
```

**Implementation requirements:**

- `Temper` constructor takes `(baseUrl, tenantId)`.
- All HTTP methods use `fetch()` with `X-Tenant-Id` header and `Content-Type: application/json`.
- `list()` does GET to `/tdata/{entityType}`. Accepts optional `{ filter, orderby, top, skip, select }` — appends as OData query params (`$filter`, `$orderby`, `$top`, `$skip`, `$select`).
- `get()` does GET to `/tdata/{entityType}('{entityId}')`.
- `create()` does POST to `/tdata/{entityType}` with JSON body.
- `action()` does POST to `/tdata/{entityType}('{entityId}')/Temper.{actionName}` with optional JSON body.
- `patch()` does PATCH to `/tdata/{entityType}('{entityId}')` with JSON body.
- All methods return parsed JSON response. On HTTP error (4xx/5xx), throw an `Error` with the response body text.
- SSE: uses browser-native `EventSource` connected to `{baseUrl}/tdata/$events`. The SSE messages are JSON. Parse each, check `entity_type` against registered listeners. `'*'` matches all.
- Auto-reconnect: EventSource handles this natively, but track `onerror`/`onopen` for status callbacks.
- Lazy SSE connection: don't connect until first `.on()` call. Disconnect when all listeners removed.
- The entire library is an IIFE that exposes `window.Temper`.
- Keep it small — target under 200 lines, under 5KB unminified.

### 2. Serve the client from Temper: update `crates/temper-server/src/routes.rs`

Add a route that serves the static JS file:

```
GET /temper-client.js → serves crates/temper-server/static/temper-client.js
```

- Use `include_str!()` to embed the JS at compile time (no filesystem dependency at runtime).
- Content-Type: `application/javascript`
- Cache-Control: `public, max-age=3600` (1hr cache, fine for dev; production behind CDN)
- Also serve at `/static/temper-client.js` as an alias.

### 3. SSE tenant filtering (if not already present)

Check `crates/temper-server/src/events.rs`. The SSE endpoint currently broadcasts ALL events. For the browser client to work properly in multi-tenant setups:

- If SSE events already include `tenant` field in the JSON payload → no change needed. Client-side filtering is sufficient for now.
- If SSE events do NOT include tenant → add the `tenant` field to the SSE event JSON payload.
- Do NOT add server-side tenant filtering via query params yet (that's a future optimization). Client-side filtering by tenant is fine for v1.

### 4. CORS headers

For local dev, apps may be served from different ports/origins. Add CORS headers to the Temper server:

- Check if CORS middleware already exists in the axum router.
- If not, add `tower-http` CORS layer with permissive defaults for dev:
  - `Access-Control-Allow-Origin: *`
  - `Access-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS`
  - `Access-Control-Allow-Headers: Content-Type, X-Tenant-Id, Authorization`
- This affects ALL routes (OData API + SSE + static).

## Usage example (what an on-the-fly app looks like)

```html
<!DOCTYPE html>
<html>
<head>
  <script src="http://localhost:3001/temper-client.js"></script>
</head>
<body>
  <div id="proposals"></div>
  <script>
    const t = new Temper('http://localhost:3001', 'haku-ops');
    
    async function render() {
      const proposals = await t.list('Proposals');
      document.getElementById('proposals').innerHTML = proposals
        .map(p => `<div>${p.Title} — ${p.Status}</div>`)
        .join('');
    }
    
    // Initial render
    render();
    
    // Live updates
    t.on('Proposal', () => render());
  </script>
</body>
</html>
```

## Files to create/modify

### Create:
- `crates/temper-server/static/temper-client.js` — the browser client library

### Modify:
- `crates/temper-server/src/routes.rs` — add static file serving route
- `crates/temper-server/Cargo.toml` — add `tower-http` with `cors` feature if not already present
- `crates/temper-server/src/lib.rs` or wherever the axum Router is assembled — add CORS layer

## Do NOT
- Use any JS build tools, bundlers, or transpilers
- Add npm/node dependencies to the temper repo
- Create a separate crate for this
- Modify the SSE event format beyond adding the tenant field (if missing)
- Add authentication to the JS client (future scope)
- Modify the OData API behavior
- Create test files for the JS (no browser test infra in this repo)
