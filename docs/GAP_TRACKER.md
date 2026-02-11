# Temper Gap Tracker

> Generated: 2026-02-11

## Blocking (must fix for basic functionality)

| # | Gap | Crate | Description | Priority |
|---|-----|-------|-------------|----------|
| 1 | `_query_options` unused in dispatch.rs | temper-server | `parse_query_options()` is called in `handle_odata_get()` but the result is bound to `_query_options` and never used. `$filter`, `$select`, `$orderby`, `$expand`, `$top`, `$skip`, `$count` are all parsed then discarded. Any OData client sending query options gets them silently ignored. | P0 |
| 2 | `$expand` parsed but not evaluated | temper-server, temper-odata | `$expand` items are fully parsed (including nested options) but never applied during entity retrieval. Navigation properties and related entities are never fetched. This is a core OData feature. | P0 |
| 3 | Entity set listing returns empty array | temper-server | `GET /EntitySet` always returns `{"value": []}`. There is no entity enumeration or collection query support. Clients cannot discover existing entities. | P0 |
| 4 | No PATCH/PUT/DELETE handlers | temper-server | Only GET and POST are implemented. Entity updates (PATCH), replacements (PUT), and deletions (DELETE) are not supported. The OData protocol requires these for CRUD operations. | P0 |
| 5 | `_body_json` unused in entity creation | temper-server | POST to entity set parses the request body into `_body_json` but ignores it. The created entity has no properties from the request -- it gets a generated UUID and default state only. | P0 |
| 6 | Entity GET returns 200 on not-found | temper-server | When an entity doesn't exist, `handle_odata_get` catches the error and returns HTTP 200 with a minimal JSON body instead of 404. This violates OData protocol and confuses clients. | P0 |
| 7 | No codegen from IOA specs | temper-codegen | `generate_entity_module()` only works with TLA+-based `SpecModel`. The primary spec format is IOA TOML, but codegen cannot consume it directly. The `temper codegen` CLI reads TLA+ files only. | P0 |

## Important (needed for real-world usage)

| # | Gap | Crate | Description | Priority |
|---|-----|-------|-------------|----------|
| 8 | `Custom(String)` effect defined but not dispatched | temper-jit | The `Effect::Custom(String)` variant exists in the transition table types, but it is never created by the IOA builder and the entity actor runtime has no mechanism to dispatch custom effects to registered hooks. The platform `hooks` module registers hooks but never receives Custom effects. | P1 |
| 9 | No event subscription mechanism | temper-server, temper-runtime | There is no way for external consumers to subscribe to entity state change events. Events are emitted as `Effect::EmitEvent` during transitions but only logged to telemetry -- no webhook, websocket, or polling mechanism for consumers. | P1 |
| 10 | No cross-entity coordination | temper-runtime, temper-server | There is no saga, choreography, or compensation mechanism for coordinating state changes across multiple entity types (e.g., Order -> Payment -> Shipment). Each entity operates in isolation. | P1 |
| 11 | No append-only collections on entities | temper-jit, temper-server | Entity state has counters and booleans but no collection/list data. There is no way to maintain an append-only list of items, line items, comments, etc. on an entity. The `items` counter tracks quantity but not the actual items. | P1 |
| 12 | No persistent evolution record storage | temper-evolution, temper-platform | The `RecordStore` trait is defined but only has an in-memory implementation for tests. O-P-A-D-I records are not persisted across server restarts. | P1 |
| 13 | No real Redis adapter | temper-store-redis | Only in-memory implementations exist for `MailboxStore`, `PlacementStore`, and `CacheStore`. The Redis client dependency (`fred`) is declared but no actual Redis-backed implementations are provided. | P1 |
| 14 | Integration webhook has no retry | temper-platform | Integration webhook delivery is fire-and-forget via `reqwest`. No retry logic, dead-letter queue, or delivery confirmation. Failed webhook deliveries are silently lost. | P1 |
| 15 | Liveness properties not verified | temper-verify | Liveness properties (temporal formulas asserting something eventually happens) are parsed from specs and stored in the model but never actually checked by the model checker or simulation. Only safety invariants are verified. | P1 |
| 16 | No Cedar policy hot-reload | temper-authz | Cedar policies are loaded at `AuthzEngine` construction time and cannot be updated without restarting. The platform has no mechanism to reload policies when specs change. | P1 |
| 17 | No entity state persistence wiring | temper-server | `ServerState` has an optional `event_store` field for Postgres, but entity actors maintain state in-memory only. There is no event sourcing loop that persists state changes to Postgres and replays on actor recovery. | P1 |
| 18 | Claude API client has no mock for testing | temper-platform | The `ClaudeClient` requires a real `ANTHROPIC_API_KEY`. There is no mock or stub implementation for testing agentic evolution without API access. Unit tests only verify construction, not actual API calls. | P1 |
| 19 | Query options ($filter, $orderby, $select) not applied | temper-server, temper-odata | All query options are fully parsed with a comprehensive parser (including nested $expand, $filter with boolean logic, $orderby with direction) but none are evaluated against entity data in the dispatch layer. | P1 |

## Nice to Have (future improvements)

| # | Gap | Crate | Description | Priority |
|---|-----|-------|-------------|----------|
| 20 | MaxCount guard never parsed | temper-spec | `Guard::MaxCount { var, max }` is defined in the IOA types but the parser never creates it. Only `MinCount`, `IsTrue`, and inline `>` comparisons are parsed. | P2 |
| 21 | Hand-rolled TOML parser is fragile | temper-spec | The IOA TOML parser is manually implemented line-by-line instead of using the `toml` crate. It handles the IOA subset correctly but cannot handle multi-line strings, escaped characters, inline tables, or nested arrays. | P2 |
| 22 | Shadow testing uses legacy API only | temper-jit | `shadow_test()` evaluates test cases with `evaluate()` (single item_count) rather than `evaluate_ctx()` (full context). Multi-counter and boolean guard comparisons are not covered by shadow tests. | P2 |
| 23 | `rule_index` lost on deserialization | temper-jit | `TransitionTable::rule_index` is `#[serde(skip)]` and defaults to empty on deserialization. Callers must remember to call `rebuild_index()` after deserializing, or `evaluate_ctx()` will return `None` for all actions. No compile-time enforcement. | P2 |
| 24 | No batch request support | temper-odata, temper-server | OData `$batch` requests are not supported. Each operation requires a separate HTTP request. | P2 |
| 25 | No `$search` support | temper-odata | Full-text search via `$search` query option is not implemented. | P2 |
| 26 | No `$apply` aggregation support | temper-odata | OData aggregation extension (`$apply`) for groupby, aggregate, filter transformations is not implemented. | P2 |
| 27 | init template uses relative path | temper-cli | `temper init` generates a Cargo.toml with `temper-runtime = { path = "../../crates/temper-runtime" }` which only works inside the Temper workspace. Not usable for standalone projects. | P2 |
| 28 | No graceful shutdown | temper-cli | The `temper serve` command runs until killed. No SIGTERM handling, connection draining, or graceful shutdown logic. | P2 |
| 29 | Optimization recommendations not applied | temper-optimize | `QueryOptimizer`, `CacheOptimizer`, and `PlacementOptimizer` generate recommendations but there is no mechanism to automatically apply them. They are analysis-only. | P2 |
| 30 | No observability provider adapters beyond ClickHouse | temper-observe | Only InMemoryStore (testing) and ClickHouseStore (production) are implemented. No Logfire, Datadog, Prometheus, or Grafana adapters. | P2 |
| 31 | Legacy TLA+ extractor is brittle | temper-spec | The TLA+ extractor uses line-by-line pattern matching, not a real parser. It works for the reference order.tla spec but would likely fail on more complex TLA+ specifications with nested definitions, LET expressions, or non-standard formatting. | P2 |
| 32 | Generated code not validated | temper-codegen | Code generation produces Rust source as text strings. The generated code is never compiled, syntax-checked, or type-checked within the codegen pipeline. Errors are only discovered when the user tries to compile. | P2 |
| 33 | No developer approval UI | temper-platform | The evolution engine creates I-Records (insights) that need developer approval (D-Records), but there is no UI, API endpoint, or CLI command for developers to review and approve/reject proposed changes. | P2 |
| 34 | Proc macros limited to marker traits | temper-macros | The `Message` and `DomainEvent` derive macros only implement empty marker traits. No validation, no generated handler code, no compile-time spec checking. | P2 |
| 35 | No Sentinel anomaly detection | temper-evolution, temper-observe | The architecture describes a Sentinel that monitors trajectory spans in ClickHouse and creates O-Records for unmet intents. This component is not implemented. | P2 |
| 36 | IncrementItems/DecrementItems legacy aliases | temper-jit | `Effect::IncrementItems` and `Effect::DecrementItems` duplicate `IncrementCounter("items")` and `DecrementCounter("items")`. Same for `Guard::ItemCountMin` vs `Guard::CounterMin`. These legacy variants add confusion. | P2 |
