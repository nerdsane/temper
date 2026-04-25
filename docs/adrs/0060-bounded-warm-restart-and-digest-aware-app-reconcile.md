# ADR-0060: Bounded Warm Restart and Digest-Aware App Reconcile

- Status: Accepted
- Date: 2026-04-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0032: Platform Store Trait and Sim Platform DST
  - ADR-0048: Dispatch Retry and Error Taxonomy
  - ADR-0057: Native Immutable File Read Plane for TemperFS
  - `crates/temper-platform/src/os_apps/mod.rs`
  - `crates/temper-server/src/platform_store.rs`
  - `crates/temper-store-turso/src/store/specs.rs`

## Context

TemperPaw production restarts exposed a structural startup problem in Temper: the server could become externally alive before all configured platform surfaces were actually usable, and warm restarts repeated expensive cold-bootstrap work.

The most visible symptoms were:

- startup OS-app dependencies were processed more than once when multiple startup apps shared the same dependency
- warm restarts reinstalled app specs, policies, WASM modules, seeded files, agents, skills, ADRs, and seed entities even when disk content had not changed
- helper paths sometimes relied on in-memory runtime indexes before restart recovery had repopulated them
- missing or malformed trigger target fields could generate invalid persistence ids such as `default:Workspace:`
- startup observability reported broad phase completion, but did not expose enough per-app skip/reconcile information to prove that warm restart was bounded

The architecture goal is not to hide startup cost behind an early liveness signal. It is to make cold bootstrap, warm restart, and durable reconcile separate paths with explicit readiness semantics.

## Decision

### 1. Cold bootstrap and warm restart are separate platform operations

Cold bootstrap remains the path that creates first-time durable platform state for a tenant.

Warm restart is a bounded reconcile path:

- recover durable platform state first
- compute the current app bundle digest from disk/source content
- compare it with durable installed-app metadata
- skip unchanged apps when registry specs are already ready
- reconcile only apps whose durable digest or recovered runtime state is stale

This keeps entity-first semantics while avoiding repeated full OS-app installation on every process restart.

### 2. Installed app metadata includes content digests

The platform store records one durable metadata row per tenant/app containing:

- app version
- bundle digest
- spec digest
- policy digest
- WASM digest
- content digest
- seed digest
- install and reconcile timestamps
- status

The bundle digest covers app manifest data, specs, CSDL, cross invariants, Cedar policies, WASM module bytes/config, `APP.md`, agents/souls, skills and companion files, system files, ADR files, and seed entities.

This metadata is intentionally stored below the process-local runtime registry. Runtime indexes can be rebuilt, but skip decisions must be anchored in durable state plus readiness of recovered specs.

### 3. Startup app dependency order is deduplicated

Startup DAG resolution uses one visited set across the entire requested startup app list.

Each app is processed once, even if it appears as a dependency of multiple startup apps. This preserves dependency ordering while preventing duplicate install/reconcile work.

### 4. Digest-aware reconcile is the public startup API

Temper exposes a reconcile operation for startup callers. The result is explicit:

- `Skipped` when durable digest matches and recovered specs are ready
- `Installed` when the app had to be installed or reconciled

Callers should emit per-app timing, skip/reconcile counts, and startup phase durations around this API instead of inferring health from process liveness.

### 5. Runtime recovery is not content bootstrap

Warm restart recovery has a runtime-only installed-app path.

That path may:

- recover Cedar policies
- reload persisted WASM modules
- inspect durable installed-app metadata
- repair in-memory and durable spec readiness when the bundle digest matches and specs are registered

That path must not write `APP.md`, agents, skills, system files, ADR files, or seed entities. If durable metadata cannot prove that an app bundle is unchanged, runtime recovery reports that reconcile is needed. The startup caller then runs digest-aware app reconcile for the required startup app surface.

This separation prevents stale verification metadata from forcing a hot full install while still allowing changed app content to reconcile intentionally.

### 6. Runtime indexes recover before content helpers

Runtime entity indexes are recovered before app reconcile can run any content bootstrap helpers.

App helpers that ensure files, directories, workspaces, agents, or seed entities exist must not make create/update decisions against an empty post-restart in-memory index. A changed or cold app may still need content bootstrap, but it now runs after durable entity state has been replayed into runtime indexes.

### 7. Empty trigger target fields are not valid entity ids

Trigger target resolution treats empty string fields as missing values.

This prevents invalid persistence ids with empty id segments and forces create-if-missing or missing-target paths to behave consistently.

## Rollout Plan

1. **Phase 0** — Add durable installed-app digest metadata, deduped startup app ordering, digest-aware reconcile, and empty trigger-target guards.
2. **Phase 1** — Move TemperPaw startup to the reconcile API and emit per-phase/per-app startup telemetry.
3. **Phase 2** — Use runtime-only installed-app recovery in the warm restart hot path, recover runtime indexes before app reconcile, and keep bulky content bootstrap behind digest-aware reconcile.
4. **Phase 3** — Gate production readiness on the configured required surfaces for each deployment while keeping `/healthz` as process liveness.
5. **Phase 4** — Move optional bulky content repair off the hot path where app semantics allow asynchronous post-ready repair.

## Readiness Gates

- A warm restart must recover durable app metadata before app helpers rely on process-local indexes.
- Runtime indexes must be recovered before any content helper performs an existence check.
- A configured startup app must be either skipped with matching digest and ready specs or reconciled successfully.
- Deployment readiness must not be satisfied by process liveness alone.
- Required transports and external user-facing surfaces must report connected/usable status before the deployment marks itself ready.

## Consequences

### Positive

- Warm restart cost becomes proportional to changed or unrecovered app state instead of total startup content size.
- App source changes are detected by content digest rather than coarse installed/not-installed checks.
- Shared startup dependencies are processed once.
- Operators get a clean skip vs reconcile signal for startup observability.
- Invalid empty target ids are rejected at the resolver boundary.

### Negative

- App install now maintains durable metadata in addition to registry state.
- Digest computation must track every content class that should participate in reconcile.
- Callers need to choose which bulky reconcile work is required before readiness and which can run after ready.

### Risks

- A content class omitted from the digest could leave stale seeded state after restart. The implementation mitigates this by hashing manifest, specs, policies, WASM, seeded content, agents, skills, system files, ADRs, and seed data.
- Runtime registry corruption with a matching durable digest could be skipped incorrectly. The reconcile API mitigates this by requiring recovered specs to be ready before skipping.
- New metadata columns must migrate existing tenant stores. The Turso migration adds nullable columns so existing rows continue to load.

### DST Compliance

- Determinism is preserved because app install/reconcile still writes through explicit platform store operations and entity/bootstrap registration.
- Digest calculation is deterministic over sorted bundle inputs.
- The new reconcile skip path is a read-only decision over durable metadata and recovered registry state.

## Non-Goals

- Replacing entity/action semantics with an external orchestrator
- Solving every session/context materialization cost in this ADR
- Making Discord or any single transport special in the Temper core
- Removing liveness checks; `/healthz` remains appropriate for process supervision

## Alternatives Considered

1. **Always reinstall startup apps on restart** — rejected because it repeats cold-bootstrap work and keeps warm restart latency tied to total app content size.
2. **Trust only in-memory runtime indexes** — rejected because indexes are process-local and may not be recovered when helper paths run.
3. **Use file mtimes for reconcile** — rejected because mtimes are not stable across build/deploy/copy flows and do not represent source-of-truth content.
4. **Move all reconcile off the hot path immediately** — rejected because readiness must still prove required specs and runtime surfaces are usable before accepting traffic.

## Rollback Policy

The reconcile API is additive. If digest-aware skip misbehaves, callers can temporarily fall back to the existing install path while keeping the metadata columns in place. The empty-target resolver guard is safe to keep because empty ids were never valid persistence ids.
