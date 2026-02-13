# WS3 — Runtime Observability

**Created**: 2026-02-12
**Completed**: 2026-02-12
**Status**: COMPLETE
**Team**: ws3-observability agent (4 tasks)

## Goal
Wire up runtime observability so that when the app is used in production, every transition, every failed intent, every anomaly is captured, queryable, and feeds into the evolution engine. Today we have the primitives (WideEvent, TrajectoryContext, O-P-A-D-I records) but they're not connected end-to-end.

## Phase 1: Entity Event History (Foundation) — COMPLETE

### Step 1: Wire event store into entity actor — DONE
Already implemented. `EntityActorHandler::handle()` persists events after transitions via `event_store.append()`. Actor recovery replays events via `replay_events()`.

### Step 2: Implement Postgres event store — DONE
Already implemented. Full `EventStore` trait implementation in temper-store-postgres with schema/migrations for `entity_events` table.

### Step 3: Complete the history API endpoint — DONE
Replaced stub at `/observe/entities/{type}/{id}/history` with real implementation:
- Path 1: If entity actor loaded in memory, reads events from actor state
- Path 2: If Postgres configured, queries `EventStore::read_events()` directly
- Path 3: Returns empty events array if neither source available
- 2 new tests (events returned, empty for unknown)

## Phase 2: Real-Time Event Streaming — COMPLETE

### Step 4: SSE endpoint for live events — DONE
- `GET /observe/events/stream` built on existing `broadcast::Sender<EntityStateChange>` from events.rs
- Supports `?entity_type=X&entity_id=Y` query params for filtering
- SSE format: `event: state_change`, `data: {entity_type, entity_id, action, status, tenant}`
- Lagged receivers gracefully skip missed events

### Step 5: Dashboard integration — DONE
WS2 implemented auto-refresh polling (5s) and connection status. SSE endpoint available for future EventSource integration.

## Phase 3: Trajectory Tracking — COMPLETE

### Step 6: Attach TrajectoryContext to all operations — DONE
- `TrajectoryLog` in state.rs with bounded `VecDeque` (ring-buffer, capacity 10,000)
- Records entity_type, entity_id, action, success, reason, timestamp for every dispatch
- Uses `sim_now()` for DST-safe timestamps

### Step 7: Failed action tracking — DONE
- Failed intents recorded with reason (guard condition, invalid state, entity not found)
- Stored in `TrajectoryLog` alongside successful transitions
- Raw material for evolution engine sentinel rules

### Step 8: Trajectory aggregation endpoint — DONE
- `GET /observe/trajectories` returns aggregated stats
- Success rate by action, most common failures, intent patterns
- `by_action` aggregation uses `BTreeMap` for deterministic JSON output

## Phase 4: Sentinel — Anomaly Detection — COMPLETE

### Step 9: Sentinel actor framework — DONE
- `SentinelActor` in `crates/temper-server/src/sentinel.rs`
- `check_rules()` evaluates all rules against trajectory log
- Uses `sim_now()`/`sim_uuid()` for DST-safe O-Record generation
- `BTreeMap` for per-action aggregation

### Step 10: Default sentinel rules — DONE
4 built-in rules:
1. **Error rate spike**: >10% failures in recent window
2. **Slow transitions**: avg duration threshold exceeded
3. **Stuck entities**: no activity beyond threshold
4. **Guard rejection rate**: high rejection rate per action

### Step 11: O-Record auto-generation — DONE
- Creates `ObservationRecord` with evidence, threshold, observed value
- Stores in `RecordStore` (Postgres-backed via WS1's PostgresRecordStore)
- Uses `sim_uuid()` for record IDs

## Phase 5: Evolution Engine Wiring — COMPLETE

### Step 12: Persistent RecordStore — DONE
Implemented by WS1 (ws1-backend). `PostgresRecordStore` in `crates/temper-evolution/src/pg_store.rs` with full CRUD, `ranked_insights()`, `update_status()`, `get_derived_records()`.

### Step 13: Evolution API endpoints — DONE
- `GET /observe/evolution/records` — list all records with filtering
- `GET /observe/evolution/records/{id}` — get single record with chain
- `POST /observe/evolution/records/{id}/decide` — developer approval (D-Record), uses `sim_now()`/`sim_uuid()`
- `GET /observe/evolution/insights` — computed insights (I-Records)

### Step 14: Dashboard evolution page — DEFERRED
Dashboard page for evolution UI deferred to future iteration. API endpoints are available for frontend integration.

## Phase 6: Metrics & Health Endpoint — COMPLETE

### Step 15: Health endpoint — DONE
- `GET /observe/health` returns: status, uptime_seconds (via `sim_now()`), specs_loaded, active_entities, transitions_total, errors_total, event_store type
- `MetricsCollector` in state.rs tracks transitions with `RwLock<BTreeMap<String, u64>>`

### Step 16: Prometheus-compatible metrics — DONE
- `GET /observe/metrics` in Prometheus text format
- `temper_transitions_total{entity_type, action, success}` — counter
- `temper_guard_rejections_total{entity_type, action}` — counter
- `temper_active_entities{entity_type}` — gauge
- Proper `# HELP`/`# TYPE` annotations

## Acceptance Criteria

- [x] Entity events persisted to Postgres, survive restart
- [x] Entity history API returns real event data
- [x] SSE stream for real-time entity events
- [x] TrajectoryContext attached to all operations
- [x] Failed intents captured and queryable
- [x] At least 2 sentinel rules running and creating O-Records — 4 rules
- [x] RecordStore persisted to Postgres
- [x] Evolution API endpoints functional
- [x] Health endpoint returns real metrics
- [x] All new code passes DST review for sim-visible changes

## Dependencies

- **Phase 1 depends on**: temper-store-postgres — DONE (already implemented)
- **Phase 2 depends on**: Phase 1 — DONE
- **Phase 3 depends on**: Phase 1 — DONE
- **Phase 4 depends on**: Phase 1 + trajectory log — DONE
- **Phase 5 depends on**: Phase 4 + WS1 PostgresRecordStore — DONE
- **Phase 6 is independent** — DONE
- **WS2 (dashboard)** benefits from all phases here — WS2 COMPLETE
