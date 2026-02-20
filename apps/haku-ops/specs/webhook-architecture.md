# Temper Webhook Architecture

## Goal
After every successful action on an entity, Temper dispatches HTTP webhooks to configured URLs. This enables external systems (like OpenClaw agents) to react to state changes in real-time without polling.

## Configuration

Webhooks are configured per-tenant in the app TOML specs directory via `webhooks.toml`:

```toml
# apps/haku-ops/specs/webhooks.toml

[[webhook]]
name = "openclaw-haku"
url = "http://127.0.0.1:18789/hooks/wake"
headers = { "Authorization" = "Bearer ${TEMPER_WEBHOOK_TOKEN}" }
# Only fire on these actions (empty = all actions)
actions = ["Select", "Deselect", "Approve", "Scratch", "WritePlan"]
# Only fire on these entity types (empty = all)
entity_types = ["Proposal"]
# Only fire on success (default: true)
on_success_only = true
# Payload template — supports ${tenant}, ${entity_type}, ${entity_id}, 
#   ${action}, ${from_status}, ${to_status}
payload_template = """
{
  "text": "[Temper] ${action} on ${entity_type} '${entity_id}' (${from_status} → ${to_status})",
  "mode": "now"
}
"""
```

## Implementation

### 1. New module: `crates/temper-server/src/webhooks.rs`

```rust
pub struct WebhookConfig {
    pub name: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub actions: Vec<String>,        // empty = all
    pub entity_types: Vec<String>,   // empty = all
    pub on_success_only: bool,
    pub payload_template: String,
}

pub struct WebhookDispatcher {
    client: reqwest::Client,
    configs: Vec<WebhookConfig>,
}

impl WebhookDispatcher {
    pub fn new(configs: Vec<WebhookConfig>) -> Self;
    
    /// Fire webhooks matching the given trajectory entry.
    /// Runs asynchronously — does NOT block the action response.
    pub async fn dispatch(&self, entry: &TrajectoryEntry);
}
```

### 2. Integration point: `state.rs` → `dispatch_tenant_action()`

After the successful trajectory persist + SSE broadcast block (~line 830-840), add:

```rust
// Fire webhooks (non-blocking)
if response.success {
    if let Some(ref dispatcher) = self.webhook_dispatcher {
        let dispatcher = dispatcher.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            dispatcher.dispatch(&entry).await;
        });
    }
}
```

### 3. Loading webhooks.toml

In `SpecRegistry::register_app()` (or wherever app specs are loaded), parse `webhooks.toml` if present. Store `WebhookDispatcher` in `ServerState`.

### 4. Template expansion

Simple string replacement:
- `${tenant}` → entry.tenant
- `${entity_type}` → entry.entity_type  
- `${entity_id}` → entry.entity_id
- `${action}` → entry.action
- `${from_status}` → entry.from_status
- `${to_status}` → entry.to_status

### 5. Environment variable expansion in headers

`${TEMPER_WEBHOOK_TOKEN}` expands from env vars at load time.

## Non-goals (for now)
- Retry logic (fire and forget; log failures)
- Webhook signing/HMAC
- Delivery guarantees (at-most-once is fine)
- UI for webhook management

## Testing
- Unit test: template expansion
- Unit test: action/entity_type filtering
- Integration test: mock HTTP server receives webhook on action
- Test: webhook failure doesn't block action response
