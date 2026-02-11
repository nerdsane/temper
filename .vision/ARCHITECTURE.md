# Temper Architecture

## Crate Dependency Graph

```
temper-spec ─────────────────┬──→ temper-verify (model checking, DST, property tests)
                             │
                             └──→ temper-jit ──→ temper-runtime ──→ temper-server
                                                      │                  │
                                                      │                  ├──→ temper-store-postgres
                                                      │                  └──→ temper-store-redis
                                                      │
                                                      └──→ temper-observe

temper-evolution (O-P-A-D-I record chain)
temper-platform (deploy pipeline, shared registry)
temper-cli (developer CLI, verify command)
```

### Crate Responsibilities

- **temper-spec**: I/O Automaton TOML parser (`.ioa.toml`) + CSDL parser. The canonical spec representation.
- **temper-verify**: Stateright model checking, deterministic simulation, property tests. Only used at design-time and in tests.
- **temper-jit**: Builds `TransitionTable` from IOA specs. No verification dependencies in production.
- **temper-runtime**: Actor system with `SimScheduler`, `SimActorSystem`, `sim_now()`/`sim_uuid()`, `TenantId`.
- **temper-server**: HTTP server hosting `EntityActor`, `EntityActorHandler`, `SpecRegistry` (multi-tenant), OData endpoints.
- **temper-observe**: WideEvent telemetry (OTEL spans + metrics), trajectory tracking for unmet intents.
- **temper-evolution**: O-P-A-D-I record chain and the Evolution Engine.
- **temper-store-postgres**: Event sourcing persistence, tenant-scoped.
- **temper-store-redis**: Mailbox and placement cache, tenant-scoped.
- **temper-platform**: Deploy pipeline orchestration, shared SpecRegistry proof.
- **temper-cli**: Developer-facing CLI including the `verify` command.

## Data Flow

### Design-Time Flow
```
Developer conversation
  → IOA spec (.ioa.toml) + CSDL (.csdl.xml) + Cedar (.cedar)
  → Verification cascade (L0 → L1 → L2 → L2b → L3)
  → TransitionTable (via temper-jit)
  → Entity actors registered in SpecRegistry
  → OData API endpoints auto-generated from CSDL
```

### Production Flow
```
User request
  → Production Chat
  → Entity actor lookup (SpecRegistry by TenantId + EntityType)
  → State transition (TransitionTable evaluation)
  → Event persisted (temper-store-postgres)
  → WideEvent telemetry emitted (temper-observe)
  → Response returned to user
```

### Evolution Loop
```
Unmet user intent (action not in current spec)
  → Trajectory span recorded
  → ClickHouse aggregation
  → Sentinel pattern detection
  → O-Record (Observation): "Users keep trying X"
  → I-Record (Insight): "Spec should support X"
  → Developer reviews and approves
  → D-Record (Decision): "Add action X with these guards"
  → Spec change → verification cascade → redeploy
```

## Multi-Tenancy

`SpecRegistry` maps `(TenantId, EntityType)` to specs + `TransitionTable`. All persistence layers (Postgres, Redis) are tenant-scoped. Single-tenant deployments use `TenantId::default()` which resolves to `"default"`.
