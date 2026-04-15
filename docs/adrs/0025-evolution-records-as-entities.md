# ADR-0025: Evolution Records & Governance Decisions as System Entities

- Status: Accepted
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0013: Evolution Loop Agent Integration
  - ADR-0014: Governance Gap Closure
  - `crates/temper-evolution/src/records.rs` (existing record types)
  - `crates/temper-evolution/src/chain.rs` (chain validation)
  - `crates/temper-server/src/state/pending_decisions.rs` (governance decisions)

## Context

Temper's platform dogfoods itself — the `temper-system` tenant manages Tenants, Projects, CatalogEntries, Collaborators, and Versions as IOA entity specs. But the evolution loop (O-P-A-D-I-FR records) and the governance decision flow (PendingDecision) are hand-coded Rust structs with manual status mutations, bypassing the very framework they help govern.

This creates a dogfooding gap:
- Evolution records use ad-hoc `RecordStore` with in-memory storage and manual chain validation
- Pending decisions use `PendingDecisionLog` with hand-coded status transitions
- Neither participates in the entity actor lifecycle (no transition tables, no invariant checking, no OData API)
- Chain integrity (O→P→A→D) is enforced by `chain.rs` rather than cross-entity state guards

## Decision

### Sub-Decision 1: Seven New System Entity Specs

Add 7 IOA entity specs to `temper-system`, replacing hand-coded record types:

1. **Observation** — Detected anomaly from production telemetry (replaces O-Record)
2. **Problem** — Formal problem statement derived from observation (replaces P-Record)
3. **Analysis** — Root cause analysis with proposed solutions (replaces A-Record)
4. **EvolutionDecision** — Human approval/rejection of proposed change (replaces D-Record)
5. **Insight** — Product intelligence from trajectory analysis (replaces I-Record)
6. **FeatureRequest** — Platform gap detected, needs developer review (replaces FR-Record)
7. **GovernanceDecision** — Cedar policy approval/denial flow (replaces PendingDecision)

**Why this approach**: These record types already have well-defined state machines (Open→Reviewed→Resolved, Pending→Approved/Denied). Making them IOA entities means they get transition tables, invariant checking, cross-entity guards, OData APIs, and event sourcing for free.

### Sub-Decision 2: Chain Enforcement via Cross-Entity Guards

Replace `chain.rs` ad-hoc validation with IOA cross-entity state guards:

- Problem.Create requires linked Observation in `Open` or `UnderReview`
- Analysis.Create requires linked Problem in `Reviewed`
- EvolutionDecision.Create requires linked Analysis in `Reviewed`

**Why this approach**: Cross-entity guards are already implemented in the framework (`resolve_cross_entity_guards` in `cross_entity.rs`). Using them for chain validation means the verification cascade can check chain integrity at spec time, not just runtime.

### Sub-Decision 3: GovernanceDecision Custom Effect

GovernanceDecision.Approve triggers a `GenerateCedarPolicy` custom effect that:
1. Reads entity fields (agent_id, action_name, resource_type, resource_id, scope)
2. Generates a Cedar permit statement
3. Validates and reloads the authz engine
4. Persists to tenant_policies

**Why this approach**: Reuses the existing custom effect dispatch pattern (`dispatch_custom_effect` in hooks.rs) already proven with `DeploySpecs`.

### Sub-Decision 4: Durable Entities with Query Pagination

Evolution records are durable (no bounded eviction). Query-level pagination via OData `$top`/`$skip` replaces the bounded in-memory store.

**Why this approach**: Evolution records form an audit trail. Eviction would break chain integrity and compliance requirements.

## Rollout Plan

1. **Phase 0** — ADR + 7 IOA specs + CSDL additions (additive, no behavior change)
2. **Phase 1** — Bootstrap registration (5→12 system entities)
3. **Phase 2** — GovernanceDecision replaces PendingDecision (highest-value change)
4. **Phase 3** — Evolution records as entities (sentinel, insight generator rewired)
5. **Phase 4** — Cleanup deprecated code paths

## Consequences

### Positive
- Full dogfooding: the governance loop is governed by the same framework it governs
- Chain integrity enforced by cross-entity guards (spec-verified, not ad-hoc)
- All evolution records get OData APIs, event sourcing, and telemetry for free
- GovernanceDecision lifecycle is spec-verified (no more manual status mutations)
- Single source of truth for entity state (entity actor system, not parallel stores)

### Negative
- 7 new specs to maintain in `temper-system` (mitigated: specs are simple state machines)
- Bootstrap time increases slightly (12 entities vs 5)
- Cross-entity guard resolution adds latency to chain creation (mitigated: budget-limited to 5 lookups)

### Risks
- Migration of existing records requires one-time script (mitigated: can be a CLI subcommand)
- Existing API consumers may depend on PendingDecision JSON shape (mitigated: phased deprecation)

### DST Compliance
- All new entities use the standard entity actor lifecycle (sim_now, sim_uuid)
- No new wall clock, random, or I/O calls introduced
- Cross-entity guards already DST-compliant (use entity state reads, no external I/O)

## Non-Goals

- Changing the Sentinel rule engine itself (it still evaluates rules, just creates entities instead of structs)
- Adding new evolution record types beyond O-P-A-D-I-FR
- Changing the Cedar policy language or evaluation engine
- Real-time streaming of evolution records (existing SSE patterns are preserved)

## Alternatives Considered

1. **Keep hand-coded stores, add OData wrappers** — Would create a second query path alongside entity actors. Rejected because it doubles maintenance without gaining spec verification.
2. **Use a separate database for evolution records** — Would break the single-tenant-scoped storage model. Rejected because entity actors already provide durable, queryable storage.
3. **Implement chain validation as a custom guard type** — Would require changes to temper-spec and temper-jit. Rejected because cross-entity state guards already handle this pattern.

## Rollback Policy

Each phase is independently revertible:
- Phase 0-1: Remove specs and revert bootstrap (no behavior change to undo)
- Phase 2: Re-enable PendingDecisionLog (kept during deprecation period)
- Phase 3: Re-enable RecordStore (kept during deprecation period)
- Phase 4: Only execute after Phase 2-3 are stable in production
