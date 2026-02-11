# Temper Crate Audit

> Generated: 2026-02-11

## Summary

- **Total crates**: 16 (+ 1 reference app)
- **Total test count**: ~471 (`#[test]` and `#[tokio::test]` annotations: 448 in crates, 23 in reference-apps)
- **Crates with full implementation**: temper-spec, temper-verify, temper-jit, temper-runtime, temper-odata, temper-authz, temper-observe, temper-evolution, temper-server, temper-cli, temper-optimize, temper-platform
- **Crates with partial implementation**: temper-codegen, temper-store-postgres, temper-store-redis
- **Crates with stubs only**: temper-macros (minimal but complete for its scope)
- **TODOs/FIXMEs found**: 0 (no TODO/FIXME markers in the codebase)
- **Dead code / `unimplemented!` / `todo!` macros**: 0 instances
- **`#[allow(dead_code)]` attributes**: 0 instances

---

## Per-Crate Findings

### temper-macros

- **Status**: Working (minimal scope)
- **Public API**:
  - `#[derive(Message)]` -- auto-implements `temper_runtime::actor::Message` for Send + 'static types
  - `#[derive(DomainEvent)]` -- auto-implements `temper_runtime::persistence::DomainEvent`
- **Dependencies**: syn, quote, proc-macro2
- **Tests**: 0 (proc macro crate -- no inline tests)
- **TODOs/FIXMEs**: None
- **Gaps**: Only two derive macros. No attribute macros for actor lifecycle, handler registration, or spec annotations. The macros simply implement marker traits -- there is no validation or code generation beyond trait impl.

---

### temper-spec

- **Status**: Working
- **Public API**:
  - `parse_automaton(toml_str) -> Result<Automaton, AutomatonParseError>` -- parse I/O Automaton TOML specs
  - `to_state_machine(automaton) -> StateMachine` -- convert IOA to legacy IR
  - `parse_csdl(xml) -> Result<CsdlDocument, CsdlParseError>` -- parse OData CSDL XML
  - `extract_state_machine(tla_source) -> Result<StateMachine, TlaExtractError>` -- legacy TLA+ extraction
  - `build_spec_model(csdl, tla_sources) -> SpecModel` -- unified model with cross-validation
  - Types: `Automaton`, `Action`, `Guard`, `Effect`, `Invariant`, `Liveness`, `Integration`, `StateVar`, `AutomatonMeta`
  - CSDL types: `CsdlDocument`, `Schema`, `EntityType`, `Property`, `NavigationProperty`, `EnumType`, `Action`, `Function`, `EntityContainer`, `EntitySet`, `Annotation`, `AnnotationValue`, `Term`
  - `StateMachine`, `Transition`, `Invariant`, `LivenessProperty`
- **Dependencies**: quick-xml, serde, serde_json, thiserror
- **Tests**: 18 tests
  - `automaton::parser::tests` -- 11 tests (parse, validate, convert to state machine, integration sections)
  - `csdl::parser::tests` -- 2 tests (reference CSDL, minimal CSDL)
  - `tlaplus::extractor::tests` -- 3 tests (reference order TLA+, module name, state set)
  - `tlaplus::extractor::debug` -- 1 debug test
  - `model::tests` -- 1 test (build_spec_model cross-validation)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - Custom TOML parser (hand-rolled, no `toml` crate dependency) -- works for the IOA subset but fragile for edge cases
  - TLA+ extractor is pattern-matching based, not a full parser -- adequate for structured specs but brittle for freeform TLA+
  - `MaxCount` guard variant defined in types but never parsed (parser only handles `>`, `min`, `is_true`)
  - `StateIn` guard variant defined but never produced by the IOA parser (guards are built in the builder, not the parser)

---

### temper-verify

- **Status**: Working
- **Public API**:
  - `VerificationCascade::from_ioa(ioa_toml)` -- orchestrates 4-level cascade
  - `check_model(model) -> VerificationResult` -- Stateright BFS model checking
  - `verify_symbolic(ioa_toml, max_counter) -> SmtResult` -- Z3 SMT verification
  - `run_simulation_from_ioa(ioa_toml, config) -> SimulationResult` -- deterministic simulation
  - `run_multi_seed_simulation_from_ioa(ioa_toml, config, seeds) -> Vec<SimulationResult>`
  - `run_prop_tests_from_ioa(ioa_toml, cases, steps) -> PropTestResult` -- property-based testing
  - `run_prop_tests_with_shrinking_from_ioa(ioa_toml, cases, steps) -> PropTestResult` -- proptest with shrinking
  - `build_model_from_ioa(ioa_toml, max_counter) -> TemperModel`
  - Types: `CascadeResult`, `CascadeLevel`, `LevelResult`, `SmtResult`, `VerificationResult`, `SimulationResult`, `PropTestResult`, `TemperModel`, `TemperModelState`, `TemperModelAction`, `ResolvedTransition`, `ModelGuard`, `ModelEffect`, `InvariantKind`
- **Dependencies**: temper-runtime, temper-spec, stateright, proptest, serde, serde_json, z3
- **Tests**: 31 tests
  - `cascade::tests` -- 3 tests (full cascade, all levels, level summaries)
  - `checker::tests` -- 2 tests (completion, all properties hold)
  - `smt::tests` -- 6 tests (guard satisfiability, unreachable states, invariant induction, dead guards, non-inductive, decrement breaks)
  - `simulation::tests` -- 7 tests (no faults, light faults, heavy faults, reproducibility, seed divergence, multi-seed, final states)
  - `proptest_gen::tests` -- 6 tests (passes, single-step, many cases, catches broken, result fields, shrinking passes/catches)
  - `model::builder::tests` -- 11 tests (states, initial state, transitions, guards, effects, invariants, liveness)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - `run_prop_tests_level` takes `_model` parameter (unused -- model is rebuilt internally)
  - Liveness properties are parsed and stored but not actively verified (no liveness checker in the model checker)
  - ActorSimulation level (Level 2b) depends on external runner callback -- no built-in actor sim implementation in this crate

---

### temper-jit

- **Status**: Working
- **Public API**:
  - `TransitionTable::from_ioa_source(ioa_toml)` -- primary constructor
  - `TransitionTable::from_automaton(automaton)` -- from parsed automaton
  - `TransitionTable::evaluate(state, item_count, action)` -- legacy single-counter eval
  - `TransitionTable::evaluate_ctx(state, ctx, action)` -- full context eval
  - `TransitionTable::rebuild_index()` -- rebuild action-name index after deserialization
  - `SwapController::new(table)` / `swap(new_table)` / `current()` / `version()`
  - `shadow_test(old, new, test_cases) -> ShadowResult`
  - Types: `TransitionTable`, `TransitionRule`, `Guard`, `Effect`, `TransitionResult`, `EvalContext`, `SwapResult`, `ShadowResult`, `Mismatch`, `TestCase`
- **Dependencies**: temper-runtime, temper-spec, serde, serde_json, thiserror; criterion (dev)
- **Tests**: 15 tests
  - `table::evaluate::tests` -- 8 tests (build, valid submit, invalid state, unknown action, guards, combinators, cancel multi-state)
  - `swap::tests` -- 4 tests (version start, increment, replace, multiple swaps)
  - `shadow::tests` -- 3 tests (identical, different detect mismatch, added rule)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - `Effect::Custom(String)` is defined but never created by the IOA builder, and there is no dispatch mechanism -- logged but not executed at runtime
  - `Effect::IncrementItems` / `DecrementItems` are legacy aliases that duplicate `IncrementCounter("items")` / `DecrementCounter("items")`
  - `Guard::ItemCountMin` is a legacy alias that duplicates `CounterMin { var: "items", min }`
  - Shadow testing only uses legacy `evaluate()` (single item_count), not `evaluate_ctx()`
  - Benchmark (`table_eval`) exists in Cargo.toml but was not verified
  - `rule_index` is `#[serde(skip)]` -- must call `rebuild_index()` after deserialization (easy to forget)

---

### temper-runtime

- **Status**: Working
- **Public API**:
  - `ActorSystem` -- main actor system type
  - `TenantId` / `QualifiedEntityId` -- multi-tenant identity
  - `actor` module: `Actor`, `ActorRef`, `ActorCell`, `ActorContext`, `Message` trait, error types
  - `mailbox` module: bounded mailbox implementation
  - `persistence` module: `EventStore` trait, `DomainEvent` trait
  - `scheduler` module: `SimScheduler`, `DeterministicRng`, `FaultConfig`, `SimActorState`, `SimMessage`
    - `clock`: `SimClock`, `WallClock`, `LogicalClock`
    - `id_gen`: `SimIdGen`, `RealIdGen`, `DeterministicIdGen`
    - `context`: `sim_now()`, `sim_uuid()`, `install_sim_context()`, `install_deterministic_context()`
    - `sim_handler`: `SimActorHandler`, `SpecInvariant`, `SpecAssert`
    - `sim_actor_system`: `SimActorSystem`, `SimActorSystemConfig`, `SimActorResult`, `ActorInvariantViolation`
  - `supervision` module: supervision strategies
  - `tenant` module: `TenantId`, `QualifiedEntityId`
- **Dependencies**: tokio, serde, serde_json, uuid, chrono
- **Tests**: ~49 tests (scheduler: 12, clock: 4, id_gen: 4, context: 5, supervision: 3, mailbox: 5, system: 4, tenant: 15, plus sim_actor_system and sim_handler tests)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - `ActorSystem` module is private (`mod system`) -- not publicly documented
  - Persistence `EventStore` trait defined but implementation lives in temper-store-postgres
  - No real networking -- all communication is in-process or simulated

---

### temper-codegen

- **Status**: Working
- **Public API**:
  - `generate_entity_module(spec_model, entity_name) -> Result<GeneratedModule, CodegenError>`
  - `GeneratedModule` { `entity_name`, `source` }
  - Internal modules: `entity` (state struct generation), `messages` (message enum generation), `state_machine` (status enum + transition table generation)
- **Dependencies**: temper-spec, syn, quote, proc-macro2, serde, serde_json
- **Tests**: 4 tests
  - `generator::tests` -- 4 tests (generates order module, contains status enum, contains messages, no stubs)
- **TODOs/FIXMEs**: None (explicitly tests that output contains no `todo!` or `unimplemented!`)
- **Gaps**:
  - Generates Rust source as strings, not via proc-macro or AST manipulation
  - Only generates from TLA+ state machines (via `build_spec_model`), not directly from IOA
  - Generated code is not compiled or validated -- just written as text files
  - No IOA-to-code path (codegen only works with the TLA+ -> StateMachine intermediate form)

---

### temper-odata

- **Status**: Working
- **Public API**:
  - `parse_path(path) -> Result<ODataPath, ODataError>` -- URL path parser
  - `parse_query_options(query_string) -> Result<QueryOptions, ODataError>` -- query string parser
  - `parse_filter(filter_str) -> Result<FilterExpr, ODataError>` -- `$filter` expression parser
  - Types: `ODataPath`, `KeyValue`, `QueryOptions`, `FilterExpr`, `BinaryOperator`, `UnaryOperator`, `ODataValue`, `OrderByClause`, `OrderDirection`, `ExpandItem`, `ExpandOptions`
  - `ODataError` error type
- **Dependencies**: serde, serde_json, thiserror
- **Tests**: 33 tests
  - `path::tests` -- 13 tests (service doc, metadata, entity set, entity key, bound action/function, navigation, composite key, raw value)
  - `query::types::tests` -- 10 tests ($select, $orderby, $top/$skip/$count, $expand simple/nested/multiple, combined, empty, invalid)
  - `query::filter::tests` -- 10 tests (simple eq, comparison ops, logical and/or/not, nested, string functions, null, precedence)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - **`$expand` parsed but not evaluated in dispatch.rs** -- `_query_options` is computed but never used
  - **`$filter` parsed but not applied** -- query options are parsed but never evaluated against entity state
  - **`$orderby` parsed but not applied** -- same as above
  - **`$select` parsed but not applied** -- same as above
  - `$top`, `$skip`, `$count` parsed but not applied
  - No `$search` support
  - No `$apply` (aggregation) support
  - No batch request (`$batch`) support

---

### temper-authz

- **Status**: Working
- **Public API**:
  - `AuthzEngine::new(policies) / evaluate(context, action, resource, attributes) -> AuthzDecision`
  - `SecurityContext` -- request security context
  - `Principal` / `PrincipalKind` -- caller identity
  - `AuthzDecision` -- Allow/Deny with reason
  - `AuthzError` error type
- **Dependencies**: cedar-policy, serde, serde_json, thiserror
- **Tests**: 10 tests
  - `engine::tests` -- 6 tests (default permit, explicit deny, role-based, action-specific, unknown action, multiple policies)
  - `context::tests` -- 4 tests (principal creation, security context, role extraction)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - No policy hot-reload mechanism
  - Cedar policies must be provided as strings at construction time
  - No integration with CSDL annotations for auto-generating Cedar policies from spec

---

### temper-observe

- **Status**: Working
- **Public API**:
  - `ObservabilityStore` trait -- SQL-like query interface over virtual tables (spans, logs, metrics)
  - `InMemoryStore` -- in-memory adapter for testing
  - `ClickHouseStore` -- ClickHouse HTTP adapter (requires running ClickHouse)
  - `WideEvent` -- structured telemetry event combining trace, metric, and log data
  - `from_transition()`, `emit_span()`, `emit_metrics()` -- WideEvent constructors
  - `TrajectoryContext` / `TrajectoryOutcome` -- trajectory tracking types
  - Schema constants: `SPAN_COLUMNS`, `LOG_COLUMNS`, `METRIC_COLUMNS`
  - `ObserveError` error type
- **Dependencies**: opentelemetry, tracing, serde, serde_json, chrono, uuid, reqwest, thiserror
- **Tests**: 28 tests
  - `schema::tests` -- 4 tests (column definitions, virtual table structure)
  - `memory::tests` -- 8 tests (insert/query spans, logs, metrics, empty results)
  - `clickhouse::tests` -- 5 tests (URL construction, query building, error handling)
  - `wide_event::tests` -- 5 tests (from_transition, emit_span, emit_metrics, serialization)
  - `trajectory::tests` -- 6 tests (context creation, outcome recording, intent matching)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - ClickHouseStore requires external ClickHouse instance -- no embedded mode
  - No Logfire, Datadog, or other provider adapters (only InMemory and ClickHouse)
  - WideEvent emission is fire-and-forget -- no delivery guarantee
  - No trajectory aggregation or pattern detection (that's in temper-evolution)

---

### temper-evolution

- **Status**: Working
- **Public API**:
  - Record types: `ObservationRecord`, `ProblemRecord`, `AnalysisRecord`, `DecisionRecord`, `InsightRecord`
  - `RecordHeader` / `RecordType` / `RecordStatus` / `RecordId`
  - `ObservationClass` / `Decision` / `InsightCategory` / `InsightSignal`
  - `RecordStore` trait -- storage interface for evolution records
  - `validate_chain(records) -> ChainValidation` -- validates O-P-A-D-I chain integrity
  - `compute_priority_score(signals) -> f64` -- priority scoring
  - `classify_insight(signals) -> InsightCategory` -- insight classification
  - `generate_digest(records) -> String` -- human-readable digest
- **Dependencies**: serde, serde_json, uuid, chrono, thiserror
- **Tests**: 20 tests
  - `records::tests` -- 7 tests (record creation, serialization, status transitions)
  - `store::tests` -- 4 tests (in-memory store operations)
  - `chain::tests` -- 3 tests (valid chain, broken chain, partial chain)
  - `insight::tests` -- 6 tests (priority scoring, classification, digest generation)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - No persistent RecordStore implementation (only trait + in-memory for tests)
  - No automatic O-Record creation from trajectory data
  - Sentinel agent (anomaly detection) not implemented in this crate
  - `generate_digest()` outputs a formatted text report but no structured output

---

### temper-store-postgres

- **Status**: Partial (trait implementations, requires Postgres)
- **Public API**:
  - `PostgresEventStore::new(pool)` -- implements `EventStore` trait
  - `migration::run_migrations(pool)` -- idempotent schema creation
  - Schema constants for table definitions
- **Dependencies**: sqlx (postgres), temper-runtime, serde, serde_json, chrono, uuid, thiserror
- **Tests**: 13 tests
  - `migration::tests` -- 1 test (SQL generation)
  - `schema::tests` -- 7 tests (table definitions, column types)
  - `store::tests` -- 5 tests (append events, read events, snapshots; most require `#[sqlx::test]` with running Postgres)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - Tests that interact with Postgres require a running database (skip in CI without Postgres)
  - No connection pooling configuration exposed
  - No tenant-scoped table partitioning (uses tenant_id column filtering)
  - No event replay or projection support

---

### temper-store-redis

- **Status**: Partial (trait + in-memory implementations)
- **Public API**:
  - `MailboxStore` trait + `InMemoryMailbox` -- actor mailbox streams
  - `PlacementStore` trait + `InMemoryPlacement` -- actor placement cache
  - `CacheStore` trait + `InMemoryCache` -- function response and entity state cache
  - Key generation utilities in `keys` module
  - `RedisStoreError` error type
- **Dependencies**: fred, serde, serde_json, thiserror
- **Tests**: 25 tests
  - `mailbox::tests` -- 4 tests (enqueue/dequeue, empty, ordering)
  - `placement::tests` -- 4 tests (register/lookup, unknown, update)
  - `cache::tests` -- 6 tests (get/set, TTL expiry, invalidation)
  - `keys::tests` -- 11 tests (key format, tenant scoping, parsing)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - Redis client (fred) is a dependency but no real Redis adapter is implemented -- only in-memory implementations exist
  - No distributed lock implementation (mentioned in module docs but not present)
  - No Redis Streams integration for mailboxes
  - Connection management not implemented

---

### temper-server

- **Status**: Working
- **Public API**:
  - `build_router(state) -> Router` -- axum router setup
  - `ServerState` -- shared server state
  - `SpecRegistry` -- multi-tenant spec + transition table registry
  - `EntityActor` / `EntityActorHandler` / `EntityMsg` / `EntityResponse` / `EntityState`
  - Dispatch: `handle_odata_get`, `handle_odata_post`, `handle_service_document`, `handle_metadata`, `handle_hints`
- **Dependencies**: temper-jit, temper-odata, temper-runtime, temper-spec, temper-authz, temper-observe, axum, hyper, tower, opentelemetry, serde_json, uuid
- **Tests**: 24 tests
  - `registry::tests` -- 9 tests (register/lookup, multi-tenant isolation, entity types, hot-swap, remove tenant, spec metadata)
  - `router::tests` -- 9 tests (service document, metadata, entity set, entity get/post, bound action, error handling)
  - `entity_actor::actor::tests` -- 7 tests (create entity, dispatch action, state transitions, invariant checking)
  - `entity_actor::sim_handler::tests` -- 7 tests (handler creation, message processing, invariant checking, spec integration)
  - Integration test (`tests/multi_tenant.rs`) -- 8 tests (multi-tenant routing, isolation, hot-swap)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - **`_query_options` unused in `handle_odata_get`** -- query options are parsed then discarded
  - **`_body_json` unused in entity creation POST** -- request body is parsed then discarded
  - `$expand` not evaluated -- related entities are not fetched
  - Entity set listing returns empty array (no entity enumeration)
  - No PATCH/PUT/DELETE handlers
  - No entity collection queries with filtering
  - Error response on entity GET when entity doesn't exist returns 200 with empty object instead of 404

---

### temper-cli

- **Status**: Working
- **Public API**: Binary crate with clap CLI
  - `temper init <name>` -- project scaffolding
  - `temper codegen [--specs-dir] [--output-dir]` -- generate Rust code from specs
  - `temper verify [--specs-dir]` -- run verification cascade
  - `temper serve [--port] [--specs-dir] [--tenant]` -- start platform server
- **Dependencies**: temper-codegen, temper-spec, temper-verify, temper-platform, temper-server, temper-store-postgres, temper-observe, clap, anyhow, tokio, sqlx
- **Tests**: 14 tests
  - `main::tests` -- 7 tests (CLI argument parsing for all commands)
  - `init::tests` -- 3 tests (namespace conversion, directory structure, exists check)
  - `codegen::tests` -- 4 tests (pascal/snake case, reference specs, missing CSDL)
  - `verify::tests` -- 1 test (reference specs verification)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - No `temper deploy` command (deployment is through the platform API)
  - No `temper evolve` command (evolution is through the platform feedback loop)
  - No `temper test` command (testing is via `cargo test`)
  - `serve` command has no graceful shutdown handling
  - `init` generates a template that references `temper-runtime` with a relative path (not publishable)

---

### temper-optimize

- **Status**: Working
- **Public API**:
  - `QueryOptimizer::new() / analyze(store) -> Vec<OptimizationRecommendation>` -- N+1 and slow query detection
  - `CacheOptimizer::new() / analyze(store) -> Vec<OptimizationRecommendation>` -- cache hit rate analysis
  - `PlacementOptimizer::new() / analyze(store) -> Vec<OptimizationRecommendation>` -- shard placement optimization
  - `SafetyChecker::validate(recommendation) -> SafetyResult` -- validates recommendations before application
  - Types: `OptimizationRecommendation`, `OptCategory`, `OptAction`, `Risk`, `SafetyResult`
- **Dependencies**: temper-observe, serde, serde_json, thiserror
- **Tests**: 15 tests
  - `lib::tests` -- 15 tests (all categories, safety checker, query N+1, slow query, cache hit rate, cache miss count, serialization roundtrips)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - Recommendations are generated but no mechanism to apply them automatically
  - `Medium` and `High` risk recommendations are always rejected by SafetyChecker (no shadow testing integration)
  - PlacementOptimizer requires observability data that may not be available in single-node deployments
  - No feedback loop -- optimizer doesn't verify if applied recommendations actually improved metrics

---

### temper-platform

- **Status**: Working
- **Public API**:
  - `bootstrap_system_tenant(state)` -- registers system entities (Project, Tenant, CatalogEntry, etc.)
  - `PlatformState` -- platform-wide shared state
  - `PlatformEvent` / `VerifyStepStatus` -- protocol messages
  - `build_platform_router(state) -> Router` -- combined platform + data API router
  - `deploy::pipeline` -- verify-and-deploy pipeline
  - `evolution` module -- feedback loop, agentic evolution agents
  - `integration` module -- webhook integration engine and registry
  - `agent::claude` -- Claude API client for agentic evolution
  - `hooks` -- post-transition hook registration
  - `optimization` -- runtime optimization wiring
- **Dependencies**: temper-server, temper-spec, temper-verify, temper-jit, temper-runtime, temper-observe, temper-evolution, temper-authz, temper-optimize, axum, reqwest, serde_json, uuid, chrono
- **Tests**: 45 tests
  - `protocol::messages::tests` -- 7 tests (message serialization, event types)
  - `deploy::pipeline::tests` -- 9 tests (deploy success, verification failure, multi-entity, empty)
  - `bootstrap::tests` -- 8 tests (system tenant registration, entity resolution)
  - `evolution::feedback::tests` -- 9 tests (O-Record creation, feedback collection, evolution events)
  - `evolution::agents::tests` -- 3 tests (agent trait, mock agents)
  - `integration::tests` -- 9 tests (webhook delivery, registry, engine)
  - `state::tests` -- 6 tests (state creation, registry access)
  - `router::tests` -- 3 tests (health endpoint, routing)
  - `optimization::tests` -- 3 tests (optimization wiring)
  - Integration tests:
    - `tests/compile_first_e2e.rs` -- 3 tests
    - `tests/integration_engine.rs` -- 9 tests
    - `tests/platform_e2e_dst.rs` -- 6 tests (E2E shared registry proof)
    - `tests/system_entity_actors.rs` -- 12 tests
    - `tests/system_entity_dst.rs` -- 23 tests (system entity deterministic simulation)
- **TODOs/FIXMEs**: None
- **Gaps**:
  - Claude API client requires `ANTHROPIC_API_KEY` -- no mock/stub for testing without API access
  - No persistent record storage for evolution chain (in-memory only)
  - Integration webhook delivery is fire-and-forget with no retry mechanism
  - No developer approval UI -- approval is conceptual only
  - `hooks` module registers post-transition hooks but Custom effects from JIT are not dispatched

---

### reference-apps/ecommerce

- **Status**: Working (reference/demo app)
- **Contents**:
  - Spec constants: `ORDER_IOA`, `PAYMENT_IOA`, `SHIPMENT_IOA`, `MODEL_CSDL`, `ORDER_CEDAR`
  - Specs: `specs/order.ioa.toml`, `specs/payment.ioa.toml`, `specs/shipment.ioa.toml`, `specs/model.csdl.xml`, `specs/policies/order.cedar`
- **Tests**: 23 tests
  - `tests/ecommerce_cascade.rs` -- 3 tests (full cascade for Order, Payment, Shipment)
  - `tests/ecommerce_dst.rs` -- 19 tests (deterministic simulation for all entities, multi-seed, heavy faults, cross-entity scenarios)
  - `tests/interactive_demo.rs` -- 1 test (end-to-end walkthrough)
- **Benchmarks**: `benches/agent_checkout.rs` -- agent checkout performance benchmark
- **Gaps**:
  - Reference app is test-only (no production binary)
  - No cross-entity coordination tests (Order -> Payment -> Shipment saga)
  - No HTTP integration tests (only spec-level verification)

---

## CLI Commands

| Command | Status | Notes |
|---------|--------|-------|
| `temper init <name>` | **Implemented** | Creates project scaffolding with CSDL template, Cargo.toml, directory structure. Fully tested. |
| `temper verify [--specs-dir]` | **Implemented** | Runs full IOA verification cascade (L0-L3) + CSDL cross-validation. Reads `.ioa.toml` and `.tla` files. Fully tested. |
| `temper codegen [--specs-dir] [--output-dir]` | **Implemented** | Generates Rust entity modules from CSDL + TLA+ specs. Generates state structs, message enums, status enums. Only works with TLA+ (not IOA directly for codegen). |
| `temper serve [--port] [--specs-dir] [--tenant]` | **Implemented** | Starts platform server with OData API, optional Postgres persistence, optional OTEL tracing, optional spec directory loading with verification. Bootstraps system tenant. |
| `temper deploy` | **Not implemented** | Deployment is through platform API (POST to deploy endpoint), not CLI. |
| `temper evolve` | **Not implemented** | Evolution is automatic through the platform feedback loop. |
| `temper test` | **Not implemented** | Testing is via `cargo test --workspace`. |
