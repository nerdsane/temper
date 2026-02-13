# Temper Gap Tracker

> Generated: 2026-02-11
> Last updated: 2026-02-12 (WS1/WS2/WS3 execution)

## Blocking (must fix for basic functionality)

| # | Gap | Crate | Status | Resolution |
|---|-----|-------|--------|------------|
| 1 | `_query_options` unused in dispatch.rs | temper-server | **RESOLVED** | `query_eval.rs` implements full $filter, $select, $orderby, $top, $skip, $count evaluation; wired into dispatch. |
| 2 | `$expand` parsed but not evaluated | temper-server, temper-odata | **RESOLVED** | `expand_entity()` resolves navigation properties via CSDL, fetches related entities, supports nested query options. |
| 3 | Entity set listing returns empty array | temper-server | **RESOLVED** | `list_entity_ids()` + `entity_index` tracks all created entities per tenant/type. |
| 4 | No PATCH/PUT/DELETE handlers | temper-server | **RESOLVED** | `handle_odata_patch`, `handle_odata_put`, `handle_odata_delete` all registered in router. DELETE removes from actor registry + entity index. |
| 5 | `_body_json` unused in entity creation | temper-server | **RESOLVED** | Body fields passed as `initial_fields` to entity actor on creation. |
| 6 | Entity GET returns 200 on not-found | temper-server | **RESOLVED** | `dispatch.rs` checks `entity_exists()` and returns 404 with OData error body. |
| 7 | No codegen from IOA specs | temper-codegen | **RESOLVED** | `build_spec_model_mixed()` handles IOA→StateMachine conversion; `generate_entity_module()` works transparently with both IOA and TLA+ specs. |

**All 7 P0 items resolved.**

## Important (needed for real-world usage)

| # | Gap | Crate | Status | Resolution |
|---|-----|-------|--------|------------|
| 8 | `Custom(String)` effect not dispatched | temper-jit, temper-server | **RESOLVED** | `custom_effects: Vec<String>` field added to `EntityResponse`. Success path collects `Effect::Custom(name)` values during transition processing. Surfaced in HTTP response JSON for downstream hook dispatch. |
| 9 | No event subscription mechanism | temper-server | **RESOLVED** | SSE endpoint at `GET /tdata/$events` with `tokio::sync::broadcast` channel. `EntityStateChange` notifications published after successful transitions. Also `GET /observe/events/stream` with entity_type/entity_id filtering. |
| 10 | No cross-entity coordination | temper-runtime, temper-server | **RESOLVED** | Choreography via reaction rules in `temper-server::reaction`. `ReactionRegistry` indexes rules per-tenant, `SimReactionSystem` for DST, `ReactionDispatcher` for async production. Bounded cascade (MAX_DEPTH=8). |
| 11 | No append-only collections | temper-jit, temper-server | **RESOLVED** | Full stack: `Guard::ListContains`, `Guard::ListLengthMin`, `Effect::ListAppend`, `Effect::ListRemoveAt` across temper-spec, temper-jit, temper-server, temper-verify. `lists: BTreeMap<String, Vec<String>>` on EntityState. |
| 12 | No persistent evolution record storage | temper-evolution | **RESOLVED** | `PostgresRecordStore` in `pg_store.rs` — `evolution_records` table with JSONB payload, indexes on `(record_type, status)` and `(derived_from)`. Full CRUD + `ranked_insights()` + `update_status()`. |
| 13 | No real Redis adapter | temper-store-redis | **RESOLVED** | `RedisMailbox` (RPUSH/LPOP/LLEN), `RedisPlacement` (GET/SET/DEL + scan), `RedisCache` (SET with EX + scan) — all via fred v10. |
| 14 | Integration webhook has no retry | temper-platform | **RESOLVED** | Exponential backoff retry in `WebhookDispatcher`. `DeadLetterQueue` trait + `InMemoryDeadLetterQueue` for permanently failed deliveries. Wired via `with_dlq()`. |
| 15 | Liveness properties not verified | temper-verify | **RESOLVED** | `LivenessViolation` type added. `check_liveness_post_simulation()` checks NoDeadlock + ReachesState. Wired into L2 cascade. `check_reaches_state()` cleaned up in stateright_impl. |
| 16 | No Cedar policy hot-reload | temper-authz | **RESOLVED** | `RwLock<PolicySet>` with atomic swap via `reload_policies()`. Invalid policies preserve existing set. `policy_count()` helper added. |
| 17 | No entity state persistence wiring | temper-server, temper-store-postgres | **RESOLVED** | `EntityActorHandler::handle()` persists events after transitions via `event_store.append()`. Actor recovery replays events via `replay_events()`. Full Postgres EventStore implementation with schema/migrations. |
| 18 | Claude API client has no mock | temper-platform | **RESOLVED** | `ChatClient` trait extracted. `MockClaudeClient` supports fixed and sequential canned responses. `ObservationAgent<C>` and `AnalysisAgent<C>` now generic with `with_client()` constructors. |
| 19 | Query options not applied | temper-server, temper-odata | **RESOLVED** | Same as #1 — `query_eval.rs` applies all parsed options to entity data. |

**All 12 P1 items resolved.**

## Nice to Have (future improvements)

| # | Gap | Crate | Status | Notes |
|---|-----|-------|--------|-------|
| 20 | MaxCount guard never parsed | temper-spec | OPEN | |
| 21 | Hand-rolled TOML parser is fragile | temper-spec | OPEN | |
| 22 | Shadow testing uses legacy API only | temper-jit | OPEN | |
| 23 | `rule_index` lost on deserialization | temper-jit | OPEN | |
| 24 | No batch request support | temper-odata, temper-server | OPEN | |
| 25 | No `$search` support | temper-odata | OPEN | |
| 26 | No `$apply` aggregation support | temper-odata | OPEN | |
| 27 | init template uses relative path | temper-cli | OPEN | |
| 28 | No graceful shutdown | temper-cli | OPEN | |
| 29 | Optimization recommendations not applied | temper-optimize | OPEN | |
| 30 | No observability provider adapters beyond ClickHouse | temper-observe | OPEN | Prometheus metrics now available at `GET /observe/metrics` (text format). |
| 31 | Legacy TLA+ extractor is brittle | temper-spec | OPEN | |
| 32 | Generated code not validated | temper-codegen | OPEN | |
| 33 | No developer approval UI | temper-platform | **PARTIALLY RESOLVED** | Evolution API endpoints added: `GET /observe/evolution/records`, `GET /observe/evolution/records/{id}`, `POST /observe/evolution/records/{id}/decide`, `GET /observe/evolution/insights`. Dashboard page deferred. |
| 34 | Proc macros limited to marker traits | temper-macros | OPEN | |
| 35 | No Sentinel anomaly detection | temper-evolution, temper-observe | **RESOLVED** | `SentinelActor` with 4 default rules: error rate spike, slow transitions, stuck entities, guard rejection rate. Auto-generates O-Records via `RecordStore`. Uses `sim_now()`/`sim_uuid()`. |
| 36 | IncrementItems/DecrementItems legacy aliases | temper-jit | OPEN | |

## Summary

| Priority | Total | Resolved | Open |
|----------|-------|----------|------|
| P0 | 7 | **7** | 0 |
| P1 | 12 | **12** | 0 |
| P2 | 17 | **2** (#33 partial, #35) | 15 |
| **Total** | **36** | **21** | **15** |
