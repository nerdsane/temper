# ADR-0019: Agentic Filesystem Navigation

- Status: Accepted
- Date: 2026-03-02
- Deciders: Temper core maintainers
- Related:
  - ADR-0015: Agent OS Cross-Entity Primitives (spawn, cross-entity guards)
  - ADR-0006: Spec-Aware Agent Interface for MCP
  - `crates/temper-server/src/query_eval.rs` (expand logic)
  - `crates/temper-odata/src/path.rs` (path parser)
  - `crates/temper-server/src/odata/read.rs` (GET handlers)
  - `crates/temper-mcp/src/tools.rs` (REPL tool dispatch)

## Context

Temper's OData surface currently has three gaps that prevent agents from navigating the entity graph as a coherent filesystem:

1. **Flat navigation only**: The `expand_entity_recursive()` function in `query_eval.rs` uses a naive convention scan (`parentId` / `{EntityType}Id`) to find related entities. This breaks for many-to-one relationships (e.g., `Order→Customer` where the FK `CustomerId` lives on Order, not Customer). The `RelationGraph` built from CSDL `ReferentialConstraint` data already contains the correct FK mappings but is completely unused by the query evaluator.

2. **No keyed collection navigation**: Parsing `/Orders('123')/Items('item-1')` loses the navigation context — `Items('item-1')` becomes `Entity("Items", "item-1")`, indistinguishable from a top-level `/Items('item-1')`. Multi-level navigation paths like `/LeadPlans('lp-1')/TestWorkflow/TestRuns` also fail because the `NavigationProperty` handler only resolves direct `Entity` parents.

3. **Entities are not self-describing**: Navigating to an entity returns its state but not what actions are available or what child entities can be navigated to. Agents must separately discover actions via `$hints`. In a filesystem metaphor, `ls -la` shows both files (data) and executables (actions) — entities should do the same.

Additionally, while ADR-0015 introduced `Effect::Spawn` and `Guard::CrossEntityState` for cross-entity orchestration, there are no IOA specs modeling the agent factory pattern (Pipeline → WorkItems → AgentSessions). The Pi extension (`--no-tools` + Temper REPL) provides the runtime, but needs entity specs to complete the agent lifecycle model.

## Decision

### Sub-Decision 1: RelationGraph-Based Navigation Expansion

Replace the naive convention scan in `expand_entity_recursive()` with proper RelationGraph lookups.

**Algorithm**:
- For non-collection nav (many-to-one, e.g. `Order→Customer`): Look up `relation_graph.outgoing[entity_type]` for an edge matching the nav property. Use the edge's `source_field` to get the FK value from the entity, then fetch the target directly.
- For collection nav (one-to-many, e.g. `Customer→Orders`): Look up `relation_graph.outgoing[target_type]` for edges where `to_entity == entity_type`. The edge's `source_field` on the target entities is the reverse FK to filter by.
- Fall back to the existing convention scan when no `ReferentialConstraint` edges exist.

**Why this approach**: The RelationGraph is already built and maintained by `build_relation_graph()` in `registry.rs`. Using it avoids duplicating CSDL relationship metadata and handles all relationship directions correctly.

### Sub-Decision 2: NavigationEntity Path Variant

Add a `NavigationEntity { parent, property, key }` variant to `ODataPath` so that `/Orders('123')/Items('item-1')` preserves the full navigation chain.

**Why this approach**: The current parser discards navigation context by returning a bare `Entity`. The new variant preserves the parent chain, enabling the GET handler to resolve the parent entity first, then navigate to the child using RelationGraph edges.

### Sub-Decision 3: Self-Describing Entity Responses

Enrich entity GET responses with two new OData annotations:
- `@odata.actions`: Actions available from the entity's current state, with OData bound action paths.
- `@odata.children`: Navigation properties from CSDL, with types and target paths.

These are computed at response time from the `TransitionTable` (for actions) and CSDL (for children).

**Why this approach**: Makes every entity self-describing without requiring separate `$hints` queries. Agents navigating to any entity immediately see what they can DO (actions/executables) and where they can GO (children/subdirectories).

### Sub-Decision 4: Polymorphic `temper.navigate()` Method

Add a single `navigate(tenant, path, params?)` method to the MCP REPL that:
- Without params: GET request to resolve entity (enriched) or collection (listed).
- With params + dot in path: POST to dispatch bound action.

**Why this approach**: Unifies read, list, and execute into one method matching the filesystem metaphor. Existing explicit methods (`get`, `list`, `action`) remain available.

### Sub-Decision 5: Agent Factory IOA Specs

Create three new IOA specs for the agent factory pattern:
- **Pipeline**: Orchestrates work — spawns WorkItems via `Effect::Spawn`.
- **WorkItem**: Individual tasks — claimed by agents, goes through review cycle.
- **AgentSession**: Agent lifecycle — assigned to work items, reports completion.

These specs use the existing `Effect::Spawn` and `Guard::CrossEntityState` primitives from ADR-0015.

**Why this approach**: Models agent orchestration as Temper entities, making agent lifecycle visible in the OData surface and governed by Cedar policies. The `watchForSpawns` mechanism in the Pi extension already watches for `entity_created` SSE events, so new specs work with existing infrastructure.

## Rollout Plan

1. **Phase 0 (This PR)** — All code changes in a single feature branch:
   - RelationGraph-based expand fix
   - NavigationEntity parser variant
   - Recursive path resolution
   - Enriched entity responses
   - `temper.navigate()` method
   - Agent factory IOA specs + CSDL + Cedar

2. **Phase 1 (Follow-up)** — Integration testing with live Pi agents running the factory workflow.

## Consequences

### Positive
- Agents can navigate `/Pipelines('p-1')/WorkItems` as a hierarchical path.
- Every entity is self-describing — agents see available actions and child paths.
- Agent lifecycle is modeled as Temper entities, governed by Cedar policies.
- Single `navigate()` method provides a coherent filesystem metaphor.

### Negative
- Enriched responses are slightly larger due to `@odata.actions` and `@odata.children` annotations.
- The `NavigationEntity` variant adds complexity to the path parser.

### Risks
- RelationGraph edges only exist for nav props with `ReferentialConstraint`. Nav props without them fall back to convention scan.

### DST Compliance
- `query_eval.rs` and `read.rs` are simulation-visible. All changes use `BTreeMap` for deterministic iteration. No wall-clock, random UUIDs, or I/O outside actor context.
- `// determinism-ok` annotations added where required (none expected for this change).

## Non-Goals

- Agent spawning runtime changes (uses existing `Effect::Spawn`).
- Multi-agent coordination protocol (handled by Pi extension + SSE).
- Pagination of enriched annotations.

## Alternatives Considered

1. **GraphQL-style schema introspection** — Would require a second query language alongside OData. Rejected because OData annotations achieve the same goal within the existing surface.
2. **Separate `/discover` endpoint** — Would fragment the API. Rejected because enriching existing entity responses is more natural for the filesystem metaphor.
3. **Convention-only FK resolution** — Would require naming conventions to be strictly followed. Rejected because CSDL `ReferentialConstraint` already provides the correct metadata.
