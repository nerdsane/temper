# WS3 — Runtime Observability

**Created**: 2026-02-12
**Status**: Seed Plan
**Team**: 1-2 agents (backend + evolution)

## Goal
Wire up runtime observability so that when the app is used in production, every transition, every failed intent, every anomaly is captured, queryable, and feeds into the evolution engine. Today we have the primitives (WideEvent, TrajectoryContext, O-P-A-D-I records) but they're not connected end-to-end.

## Current State

### What Exists (Implemented)
- **WideEvent model** — unified telemetry primitive with tags/attributes/measurements
- **Dual-view emission** — `emit_span()` + `emit_metrics()` from every entity actor transition
- **OTEL integration** — TracerProvider + MeterProvider with OTLP/HTTP export, no-op safe
- **ClickHouseStore** — query adapter for ClickHouse (read-only, write via OTEL SDK)
- **InMemoryStore** — for testing
- **TrajectoryContext/Outcome** — metadata types for trajectory tracking
- **O-P-A-D-I record types** — full type definitions, chain validation, insight computation
- **RecordStore** — in-memory only (lost on restart)
- **Observe API** — `/observe/specs`, `/observe/entities`, `/observe/verify/{entity}`, `/observe/simulation/{entity}`
- **Entity history endpoint** — STUB (`/observe/entities/{type}/{id}/history`)

### What's Missing
- **Entity history** — stub response, needs temper-store-postgres wiring
- **Sentinel actors** — no anomaly detection
- **Unmet intent capture** — no mechanism for production chat failures
- **Evolution engine loop** — record types exist but aren't auto-generated
- **Production persistence** — records lost on restart
- **Real-time event streaming** — no WebSocket/SSE for live events
- **Dashboard integration** — frontend exists but can't show runtime data

## Phase 1: Entity Event History (Foundation)

Without event persistence, nothing else works. This is the foundation.

### Step 1: Wire event store into entity actor
**Crate**: temper-server
**Current state**: `ServerState` has `event_store: Option<Arc<dyn EventStore>>` but it's never used.
**Fix**: In `EntityActorHandler::handle()`, after a successful transition:
1. Build an `EntityEvent` from the WideEvent
2. Persist via `event_store.append(entity_type, entity_id, event)`
3. On actor recovery, replay events to rebuild state

**Files**:
- `crates/temper-server/src/entity_actor/actor.rs`
- `crates/temper-store-postgres/src/`

### Step 2: Implement Postgres event store
**Crate**: temper-store-postgres
**Schema**:
```sql
CREATE TABLE entity_events (
    id BIGSERIAL PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_num BIGINT NOT NULL,
    action_name TEXT NOT NULL,
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    params JSONB,
    timestamp TIMESTAMPTZ NOT NULL,
    trace_id TEXT,
    UNIQUE(tenant_id, entity_type, entity_id, sequence_num)
);
CREATE INDEX idx_events_entity ON entity_events(tenant_id, entity_type, entity_id);
CREATE INDEX idx_events_time ON entity_events(timestamp);
```

### Step 3: Complete the history API endpoint
**Crate**: temper-server
**Current**: Stub at `/observe/entities/{type}/{id}/history`
**Fix**: Query the Postgres event store and return the event stream.
**Response shape**:
```json
{
  "entity_type": "Issue",
  "entity_id": "abc-123",
  "events": [
    {
      "sequence": 1,
      "action": "AcceptTriage",
      "from_state": "Triage",
      "to_state": "Backlog",
      "timestamp": "2026-02-12T10:30:00Z",
      "params": {}
    }
  ]
}
```

## Phase 2: Real-Time Event Streaming

### Step 4: SSE endpoint for live events
**Crate**: temper-server
**Endpoint**: `GET /observe/events/stream`
**Behavior**: Server-Sent Events stream that emits every entity transition in real-time.
**Implementation**: After emit_span/emit_metrics in the actor, also publish to a broadcast channel. The SSE endpoint subscribes to this channel.
**Query params**: `?entity_type=Issue&entity_id=abc` for filtering

### Step 5: Dashboard integration
- Dashboard (WS2) subscribes to SSE stream
- Entity list auto-updates when entities change state
- Entity inspector shows new events appearing in real-time
- Add "Live" indicator when connected to event stream

## Phase 3: Trajectory Tracking

### Step 6: Attach TrajectoryContext to all operations
**Crate**: temper-server
**Current**: TrajectoryContext exists as a type but isn't used.
**Fix**: For every incoming HTTP request:
1. Extract or generate a trace_id
2. Build TrajectoryContext (trace_id, turn_number from sequence, user_intent from action name)
3. Pass context through to the entity actor
4. Include in WideEvent emission
5. On request completion, build TrajectoryOutcome

### Step 7: Failed action tracking
**Crate**: temper-server
**Current**: Failed actions (guard not met, invalid action) emit WideEvents with success=false.
**Fix**: Additionally:
1. Record the failed intent (what the user tried to do)
2. Record the reason (guard condition, invalid state)
3. Store in a `failed_intents` table or append to trajectory log
4. This is the raw material for the evolution engine

### Step 8: Trajectory aggregation endpoint
**Crate**: temper-server
**Endpoint**: `GET /observe/trajectories`
**Returns**: Aggregated trajectory stats — success rate by action, most common failures, intent patterns
**Supports**: `$filter` by entity_type, time range, success/failure

## Phase 4: Sentinel — Anomaly Detection

### Step 9: Sentinel actor framework
**Crate**: temper-evolution (or new temper-sentinel)
**What it is**: A background actor that periodically queries the observability store and creates O-Records when thresholds are crossed.
**Design**:
```rust
pub struct SentinelActor {
    store: Arc<dyn ObservabilityStore>,
    record_store: Arc<dyn RecordStore>,
    rules: Vec<SentinelRule>,
    check_interval: Duration,
}

pub struct SentinelRule {
    pub name: String,
    pub query: String,              // SQL against virtual tables
    pub threshold: f64,
    pub classification: ObservationClass,
    pub window: Duration,           // Look-back window
}
```

### Step 10: Default sentinel rules
Built-in rules that ship with Temper:
1. **Error rate spike**: `SELECT count(*) as errors FROM spans WHERE success = false AND timestamp > now() - interval '5 minutes'` — threshold: >10% of total
2. **Slow transitions**: `SELECT avg(duration_ns) FROM spans WHERE operation = '{action}' AND timestamp > now() - interval '5 minutes'` — threshold: >1s avg
3. **Stuck entities**: `SELECT entity_id FROM spans GROUP BY entity_id HAVING max(timestamp) < now() - interval '1 hour'` — entities with no activity
4. **Guard rejection rate**: `SELECT operation, count(*) FROM spans WHERE success = false GROUP BY operation` — high rejection = UX problem

### Step 11: O-Record auto-generation
When a sentinel rule triggers:
1. Create an `ObservationRecord` with the evidence query, threshold, observed value
2. Store in RecordStore
3. Emit a WideEvent for the observation itself (meta-observability)
4. Surface in the dashboard

## Phase 5: Evolution Engine Wiring

### Step 12: Persistent RecordStore
**Crate**: temper-evolution
**Current**: In-memory only
**Fix**: Postgres-backed implementation:
```sql
CREATE TABLE evolution_records (
    id TEXT PRIMARY KEY,
    record_type TEXT NOT NULL,  -- O, P, A, D, I
    status TEXT NOT NULL,
    created_by TEXT,
    derived_from TEXT,
    timestamp TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL
);
CREATE INDEX idx_records_type ON evolution_records(record_type, status);
CREATE INDEX idx_records_derived ON evolution_records(derived_from);
```

### Step 13: Evolution API endpoints
**Crate**: temper-server
**Endpoints**:
- `GET /observe/evolution/records` — list all records with filtering
- `GET /observe/evolution/records/{id}` — get single record with chain
- `POST /observe/evolution/records/{id}/decide` — developer approval (D-Record)
- `GET /observe/evolution/insights` — computed insights (I-Records)

### Step 14: Dashboard evolution page
New page in the dashboard:
- List of open O-Records (anomalies detected)
- Drill-down into O→P→A chain
- Approval/rejection UI for D-Records
- Insight digest with priority scores

## Phase 6: Metrics & Health Endpoint

### Step 15: Health endpoint
**Endpoint**: `GET /observe/health`
**Returns**:
```json
{
  "status": "healthy",
  "uptime_seconds": 3600,
  "specs_loaded": 4,
  "active_entities": 15,
  "transitions_total": 1234,
  "errors_total": 5,
  "event_store": "postgres",
  "otel_export": "active"
}
```

### Step 16: Prometheus-compatible metrics
**Endpoint**: `GET /observe/metrics`
**Format**: Prometheus text format
**Metrics**:
- `temper_transitions_total{entity_type, action, success}` — counter
- `temper_transition_duration_seconds{entity_type, action}` — histogram
- `temper_active_entities{entity_type}` — gauge
- `temper_guard_rejections_total{entity_type, action}` — counter

## Acceptance Criteria

- [ ] Entity events persisted to Postgres, survive restart
- [ ] Entity history API returns real event data
- [ ] SSE stream for real-time entity events
- [ ] TrajectoryContext attached to all operations
- [ ] Failed intents captured and queryable
- [ ] At least 2 sentinel rules running and creating O-Records
- [ ] RecordStore persisted to Postgres
- [ ] Evolution API endpoints functional
- [ ] Health endpoint returns real metrics
- [ ] All new code passes DST review for sim-visible changes

## Dependencies

- **Phase 1 depends on**: temper-store-postgres having a working connection (may need DATABASE_URL setup)
- **Phase 2 depends on**: Phase 1 (events must be captured before they can be streamed)
- **Phase 3 depends on**: Phase 1 (trajectory needs event persistence)
- **Phase 4 depends on**: Phase 1 + OTEL or ClickHouse running
- **Phase 5 depends on**: Phase 4 (sentinels create O-Records that feed evolution)
- **Phase 6 is independent** — can start anytime
- **WS2 (dashboard)** benefits from all phases here (more data to display)
