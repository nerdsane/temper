# Temper: End-to-End Implementation Plan

This plan completes the remaining ~40% of the Temper vision — wiring real
infrastructure, building a live agent, and closing the feedback loops.

**Status**: 240 tests passing, 16 crates, server functional with in-memory actors.
**Goal**: Fully operational system with persistent state, real observability,
a live LLM agent, trajectory intelligence, and evolution records.

---

## Phase 1: Infrastructure (Docker Compose)

### 1.1 Create `docker-compose.yml`

```yaml
services:
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: temper
      POSTGRES_USER: temper
      POSTGRES_PASSWORD: temper_dev
    ports:
      - "5432:5432"
    volumes:
      - postgres_data:/var/lib/postgresql/data

  redis:
    image: redis:7-alpine
    ports:
      - "6379:6379"

  clickhouse:
    image: clickhouse/clickhouse-server:24-alpine
    ports:
      - "8123:8123"  # HTTP interface
      - "9000:9000"  # Native protocol
    volumes:
      - clickhouse_data:/var/lib/clickhouse

volumes:
  postgres_data:
  clickhouse_data:
```

### 1.2 Create `scripts/init-db.sql`

Run `temper-store-postgres::migration::run_migrations()` on startup.
Also create ClickHouse tables for observability (spans, logs, metrics).

### 1.3 Add `.env` for local development

```
DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper
REDIS_URL=redis://localhost:6379
CLICKHOUSE_URL=http://localhost:8123
```

### 1.4 Update reference app `Cargo.toml`

Add `temper-store-postgres`, `temper-store-redis`, `temper-observe` as deps.

---

## Phase 2: Wire Real Persistence

### 2.1 Connect PostgresEventStore to entity actors

Currently: entity actors hold state in memory, events in a Vec.
Target: events persisted to Postgres, state rebuilt on actor restart.

**Changes to `temper-server/src/entity_actor/actor.rs`:**
- Add `event_store: Arc<PostgresEventStore>` to `EntityActor`
- In `pre_start`: load latest snapshot + replay events from journal
- After each transition: append event to Postgres via `event_store.append()`
- Periodically snapshot state (every 100 events)

**Changes to `temper-server/src/state.rs`:**
- `ServerState::with_postgres()` constructor that takes a `PgPool`
- Pass the pool to each `EntityActor` on spawn

**DST-first**: Write tests that exercise the persistence round-trip:
- Create actor, apply transitions, stop actor, restart actor, verify state rebuilt

### 2.2 Connect Redis for mailbox + cache

**Changes to `temper-server/src/state.rs`:**
- Add `redis_pool: Arc<fred::clients::RedisPool>` to ServerState
- Use `temper-store-redis::InMemoryCache` for function response caching
  (swap to real Redis cache when connection is available)

**Changes to entity actor:**
- Cache GET responses in Redis with TTL
- Invalidate cache on state change

### 2.3 Run migrations on startup

**Changes to `reference/ecommerce/src/main.rs`:**
```rust
// Connect to Postgres
let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;
temper_store_postgres::migration::run_migrations(&pool).await?;

// Connect to Redis
let redis = RedisClient::new(RedisConfig::from_url(&env::var("REDIS_URL")?)?);
redis.connect();

// Build state with real persistence
let state = ServerState::with_infrastructure(system, csdl, csdl_xml, tla_sources, pool, redis);
```

---

## Phase 3: ClickHouse Observability Adapter

### 3.1 Create ClickHouse tables

```sql
CREATE TABLE IF NOT EXISTS spans (
    trace_id String,
    span_id String,
    parent_span_id Nullable(String),
    service String,
    operation String,
    status String,
    duration_ns UInt64,
    start_time DateTime64(9),
    end_time DateTime64(9),
    attributes String  -- JSON
) ENGINE = MergeTree()
ORDER BY (service, start_time);

CREATE TABLE IF NOT EXISTS logs (
    timestamp DateTime64(9),
    level String,
    service String,
    message String,
    attributes String  -- JSON
) ENGINE = MergeTree()
ORDER BY (service, timestamp);

CREATE TABLE IF NOT EXISTS metrics (
    metric_name String,
    timestamp DateTime64(9),
    value Float64,
    tags String  -- JSON
) ENGINE = MergeTree()
ORDER BY (metric_name, timestamp);
```

### 3.2 Implement ClickHouse adapter

**New file: `crates/temper-observe/src/adapters/clickhouse.rs`**

Implement `ObservabilityStore` trait against ClickHouse HTTP API:
- `query_spans()` → `SELECT ... FROM spans WHERE ...`
- `query_logs()` → `SELECT ... FROM logs WHERE ...`
- `query_metrics()` → `SELECT ... FROM metrics WHERE ...`

Use `reqwest` for HTTP calls to ClickHouse.

### 3.3 Emit spans from the server

**Add OTLP span emission to entity actor:**
- On every transition: emit a span with actor_type, entity_id, action, from_state, to_state, duration
- On every HTTP request: emit a span with method, path, status, duration
- Write spans to ClickHouse via the adapter

**Add tracing middleware to axum:**
- Use `tower-http::trace::TraceLayer` (already in deps)
- Configure to emit structured spans compatible with the canonical schema

---

## Phase 4: Build the LLM Agent

### 4.1 Agent architecture

Create `reference/ecommerce/src/agent/` module:

```
agent/
├── mod.rs           # Agent trait + orchestrator
├── customer.rs      # CustomerAgent — handles customer requests
├── operations.rs    # OperationsAgent — processes orders
└── client.rs        # OData HTTP client for the agent
```

### 4.2 OData client

```rust
pub struct TemperClient {
    base_url: String,
    http: reqwest::Client,
    principal_id: String,
    principal_kind: String,
    agent_role: String,
}

impl TemperClient {
    pub async fn get_entity(&self, set: &str, id: &str) -> Result<Value>;
    pub async fn create_entity(&self, set: &str, body: &Value) -> Result<Value>;
    pub async fn invoke_action(&self, set: &str, id: &str, action: &str, params: &Value) -> Result<Value>;
    pub async fn get_metadata(&self) -> Result<String>;
}
```

### 4.3 CustomerAgent

```rust
pub struct CustomerAgent {
    client: TemperClient,
    llm: Box<dyn LlmProvider>,  // Claude or OpenAI
    trajectory: TrajectoryContext,
}

impl CustomerAgent {
    /// Process a natural language customer request.
    /// Reads $metadata hints, plans actions, executes via OData API.
    pub async fn handle_request(&mut self, user_input: &str) -> AgentResponse {
        // 1. Read $metadata for available actions + Agent.Hint annotations
        // 2. Send to LLM with system prompt + metadata + user input
        // 3. Parse LLM response for tool calls (OData operations)
        // 4. Execute each tool call via TemperClient
        // 5. If action fails (409), feed error back to LLM for recovery
        // 6. Return final response to user
    }
}
```

### 4.4 LLM provider abstraction

```rust
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message]) -> Result<String>;
}

pub struct ClaudeProvider { api_key: String }
pub struct OpenAiProvider { api_key: String }
```

### 4.5 Agent system prompt

```
You are an e-commerce customer service agent powered by the Temper API.

Available entity sets: {from $metadata}
Available actions: {from $metadata, with Agent.Hint annotations}

Rules:
- Always GET the entity first to check its current status
- Read the Agent.Hint annotation before calling any action
- If an action returns 409 Conflict, do NOT retry — read the error and suggest alternatives
- For shipped orders, use InitiateReturn instead of CancelOrder

Available tools:
- get_entity(set, id) → GET /odata/{set}('{id}')
- create_entity(set, body) → POST /odata/{set}
- invoke_action(set, id, action, params) → POST /odata/{set}('{id}')/Temper.Ecommerce.{action}
```

---

## Phase 5: Trajectory Capture

### 5.1 Add trajectory headers to agent client

Every OData call from the agent includes:
```
X-Temper-Trajectory: trace_id={uuid},turn={n}
X-Temper-Agent: prompt_version=v1,model=claude-sonnet-4-5-20250929
```

### 5.2 Server-side trajectory recording

**Add trajectory middleware to `temper-server`:**
- Extract `X-Temper-Trajectory` header
- Create parent span for the trajectory
- Record each OData operation as a child span
- On response: include trajectory context in span attributes

**Write trajectory spans to ClickHouse:**
```sql
INSERT INTO spans (trace_id, span_id, service, operation, status,
                   duration_ns, start_time, end_time, attributes)
VALUES (?, ?, 'temper', 'odata.POST.SubmitOrder', 'ok', ?, ?, ?,
        '{"entity_type":"Order","entity_id":"...","trajectory.turn":2}')
```

### 5.3 Feedback endpoint

Wire the existing `SubmitFeedback` action to write to ClickHouse:
```sql
INSERT INTO metrics (metric_name, timestamp, value, tags)
VALUES ('trajectory.feedback_score', now(), 0.8,
        '{"trace_id":"...","signal":"task_completed"}')
```

---

## Phase 6: Trajectory Analysis

### 6.1 Trajectory analyzer queries

Implement the SQL queries from the paper as actual ClickHouse queries:

**Unmet intents:**
```sql
SELECT attributes->>'user_intent' AS intent,
       count(*) AS attempts,
       avg(CASE WHEN status = 'error' THEN 1 ELSE 0 END) AS failure_rate
FROM spans
WHERE service = 'temper' AND operation LIKE 'trajectory%'
GROUP BY intent
HAVING failure_rate > 0.5
ORDER BY attempts DESC
```

**Friction patterns:**
```sql
SELECT attributes->>'user_intent' AS intent,
       avg(countIf(operation LIKE 'odata%')) AS avg_calls
FROM spans
WHERE service = 'temper'
GROUP BY trace_id, intent
HAVING avg_calls > 3
```

### 6.2 Sentinel actor

**New: `reference/ecommerce/src/sentinel.rs`**

A background actor that runs periodically:
1. Query ClickHouse for anomalies (latency spikes, error rate increases)
2. Query ClickHouse for trajectory patterns (unmet intents, friction)
3. Generate O-Records and I-Records
4. Store in the `RecordStore`

```rust
pub struct SentinelActor {
    observe_store: Arc<dyn ObservabilityStore>,
    record_store: RecordStore,
    interval: Duration,
}
```

---

## Phase 7: Evolution Engine Live

### 7.1 Wire evolution records to Git

Store records as TOML files in `evolution/` directory:
- On O-Record creation: write to `evolution/observations/O-{id}.toml`
- On I-Record creation: write to `evolution/insights/I-{id}.toml`

### 7.2 Product intelligence digest

Run `generate_digest()` on a schedule and print to logs:
```
TEMPER PRODUCT INTELLIGENCE — Weekly Digest
============================================
UNMET INTENTS:
  #1 "split order" — 234 attempts, 86% fail
     → Need: SplitOrder action
```

### 7.3 Human review workflow

When a sentinel generates an O-Record with high severity:
1. Log a structured alert
2. Write the record chain to `evolution/`
3. In a real deployment: create a Git PR or Slack notification

---

## Phase 8: End-to-End Demo Script

### 8.1 `scripts/demo.sh`

```bash
#!/bin/bash
# Start infrastructure
docker compose up -d
sleep 5

# Start the server
cargo run -p ecommerce &
sleep 3

# Run the agent through several scenarios
cargo run -p ecommerce -- agent "Create an order with 2 widgets"
cargo run -p ecommerce -- agent "What's the status of my order?"
cargo run -p ecommerce -- agent "Cancel my order"
cargo run -p ecommerce -- agent "Ship my order" # Should fail — already cancelled
cargo run -p ecommerce -- agent "I want to split my order" # Unmet intent

# Show trajectory analysis
cargo run -p ecommerce -- analyze-trajectories

# Show evolution records
ls -la evolution/observations/
ls -la evolution/insights/

# Show product intelligence digest
cargo run -p ecommerce -- digest

# Cleanup
docker compose down
```

---

## Dependency Order

```
Phase 1 (Docker Compose)     — no code deps, just infra
    ↓
Phase 2 (Persistence)        — needs Phase 1 running
    ↓
Phase 3 (ClickHouse)         — needs Phase 1 running
    ↓
Phase 4 (LLM Agent)          — needs Phase 2 (working server with persistence)
    ↓                           needs API keys: ANTHROPIC_API_KEY or OPENAI_API_KEY
Phase 5 (Trajectory Capture) — needs Phase 3 (ClickHouse) + Phase 4 (agent)
    ↓
Phase 6 (Analysis)           — needs Phase 5 (trajectory data in ClickHouse)
    ↓
Phase 7 (Evolution Engine)   — needs Phase 6 (analysis producing records)
    ↓
Phase 8 (Demo)               — needs all above

Parallelizable: Phase 2 + Phase 3 (both just need Docker)
Parallelizable: Phase 4 + Phase 5 (agent + trajectory capture)
```

## New Dependencies Needed

```toml
# In workspace Cargo.toml
reqwest = { version = "0.12", features = ["json"] }      # ClickHouse HTTP + LLM API calls
clickhouse = { version = "0.13", features = ["time"] }    # ClickHouse native client (optional)
dotenvy = "0.15"                                          # .env file loading
```

## API Keys Required

- `ANTHROPIC_API_KEY` — for Claude-based agent (Phase 4)
- OR `OPENAI_API_KEY` — for OpenAI-based agent (Phase 4)
- Neither needed until Phase 4

## Estimated Effort

| Phase | Effort | Can Parallelize |
|-------|--------|-----------------|
| 1. Docker Compose | Small | — |
| 2. Persistence wiring | Medium | With Phase 3 |
| 3. ClickHouse adapter | Medium | With Phase 2 |
| 4. LLM Agent | Large | With Phase 5 |
| 5. Trajectory capture | Medium | With Phase 4 |
| 6. Trajectory analysis | Medium | — |
| 7. Evolution Engine | Medium | — |
| 8. Demo script | Small | — |
