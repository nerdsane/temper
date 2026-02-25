# Crate Refactor Plan (2026-02-24)

## Why

The workspace has grown to ~60k LOC, with the biggest concentration in `temper-server`.
Current priorities are:

1. Reduce bloat and line count.
2. Make behavior easier to read and reason about.
3. Close semantic gaps where implementation and intent diverge.

## Baseline Size (Rust LOC)

| Crate | LOC | Files |
|---|---:|---:|
| temper-server | 15,591 | 43 |
| temper-platform | 6,365 | 27 |
| temper-spec | 4,023 | 16 |
| temper-verify | 3,287 | 10 |
| temper-runtime | 3,177 | 18 |
| temper-cli | 2,815 | 9 |

Largest single files:

- `crates/temper-server/src/observe/mod.rs` (1234)
- `crates/temper-server/src/dispatch.rs` (1191)
- `crates/temper-mcp/src/lib.rs` (1151)

## Findings

### High

1. SMT guard semantics are currently over-approximate for state/list value checks.
   - `ModelGuard::StateIn` in SMT encodes only `!states.is_empty()` instead of status membership.
   - `ModelGuard::ListContains` in SMT encodes `len > 0` and ignores requested value.
   - Files:
     - `crates/temper-verify/src/smt.rs:353`
     - `crates/temper-verify/src/smt.rs:379`
   - Impact: symbolic verification can report false results versus runtime/Stateright semantics.

### Medium

2. OData handler logic is duplicated across HTTP verbs in one large module.
   - Repeated: tenant/entity-set resolution, verification gate checks, entity existence checks, response shaping.
   - File:
     - `crates/temper-server/src/dispatch.rs:150`
     - `crates/temper-server/src/dispatch.rs:532`
     - `crates/temper-server/src/dispatch.rs:811`
     - `crates/temper-server/src/dispatch.rs:949`
     - `crates/temper-server/src/dispatch.rs:1067`
   - Impact: behavior drift risk and hard-to-review changes.

3. Metadata persistence backend fan-out is hand-coded repeatedly.
   - Same Postgres/Turso/Redis branching in each operation.
   - File:
     - `crates/temper-server/src/state/persistence.rs:19`
     - `crates/temper-server/src/state/persistence.rs:103`
     - `crates/temper-server/src/state/persistence.rs:156`
     - `crates/temper-server/src/state/persistence.rs:212`
   - Impact: repetitive control flow and inconsistent error handling over time.

4. Variable-initial parsing logic is duplicated across crates.
   - File:
     - `crates/temper-mcp/src/lib.rs:451`
     - `crates/temper-verify/src/model/builder.rs:178`
     - `crates/temper-verify/src/model/builder.rs:185`
   - Impact: parse behavior can diverge by surface area.

### Low

5. Test-heavy modules keep prod and tests mixed in very large files.
   - File:
     - `crates/temper-server/src/observe/mod.rs:256`
     - `crates/temper-server/src/router.rs:88`
     - `crates/temper-mcp/src/lib.rs:881`
   - Impact: increases navigation and review friction.

## Refactor Program

## Phase 1 (Quick Wins, low risk)

1. Extract OData shared helpers from `dispatch.rs`.
   - Add helpers:
     - `resolve_entity_context(...)`
     - `guard_write_access(...)`
     - `load_entity_or_404(...)`
     - `shape_entity_response(...)`
   - Goal: reduce `dispatch.rs` by 200-300 LOC without changing behavior.

2. Introduce backend dispatcher helper in persistence.
   - Centralize backend selection into one internal function.
   - Keep explicit Redis ephemeral errors, but remove repeated branch scaffolding.
   - Goal: reduce `state/persistence.rs` by 80-120 LOC.

3. Move large inline tests into dedicated integration/unit test files.
   - Split `observe/mod.rs`, `router.rs`, and `temper-mcp/src/lib.rs` test sections.
   - Goal: smaller production files and clearer module boundaries.

## Phase 2 (Semantic Alignment)

1. Make SMT status semantics explicit.
   - Add symbolic status variable.
   - Encode `StateIn` and transition constraints using status membership.
   - Align guard encoding with `stateright_impl` semantics.

2. Make list semantics explicit in SMT.
   - Keep current bounded abstraction but encode when value-sensitive checks are approximated.
   - Option A: model membership as symbolic finite set.
   - Option B: declare approximation mode and downgrade assertion confidence.

3. Unify guard/effect semantics in one shared adapter.
   - Create shared evaluator/normalizer module used by both Stateright and SMT pipelines.
   - Remove duplicate logic branches in `stateright_impl.rs` and `smt.rs`.

## Phase 3 (Crate Boundary Cleanup)

1. `temper-server`: split into focused OData modules.
   - `odata/read.rs`
   - `odata/write.rs`
   - `odata/bindings.rs`
   - `odata/response.rs`

2. `temper-mcp`: split monolith `lib.rs`.
   - `protocol.rs` (JSON-RPC framing)
   - `tools.rs` (tool defs)
   - `runtime.rs` (dispatch and HTTP bridge)
   - `convert.rs` (Monty/JSON transforms)

3. `temper-platform` tests: shared fixtures/harness.
   - Consolidate repeated setup into `tests/common/`.
   - Reduce duplication in large system tests.

## Coding Rules to Lock In

1. No silent fallback success for missing implementations.
   - Return explicit capability/unsupported errors.

2. One semantic source of truth for guards/effects.
   - If symbolic execution approximates, it must be explicit in API/result fields.

3. Max target file size for production modules.
   - Soft limit: 400 LOC.
   - Hard review warning above 600 LOC.

## Execution Order

1. Phase 1.1 OData helper extraction.
2. Phase 1.2 persistence backend dispatch extraction.
3. Phase 1.3 test splitting.
4. Phase 2 SMT semantic alignment.
5. Phase 3 crate/module boundary reorganization.

## Expected Outcome

- ~10-20% reduction in core server module LOC.
- Lower duplication in handler and persistence pathways.
- Clearer behavior guarantees between runtime, Stateright, and SMT.
- Faster onboarding for contributors via smaller, purpose-focused modules.

## Completion Status (2026-02-24)

### Phase 1 (Quick Wins)

- [x] OData helper extraction and split from `dispatch.rs`.
  - Completed via `crates/temper-server/src/odata/{read,write,common}.rs`.
  - Additional follow-up split completed in Phase 3 (`bindings.rs`, `response.rs`).
- [x] Persistence backend dispatch extraction.
  - Completed in `crates/temper-server/src/state/persistence.rs` via centralized metadata backend selection.
- [x] Large inline test extraction.
  - Completed for:
    - `crates/temper-server/src/router.rs` → `router_tests.rs`
    - `crates/temper-server/src/observe/mod.rs` → `mod_tests.rs`
    - `crates/temper-mcp/src/lib.rs` → `lib_tests.rs`

### Phase 2 (Semantic Alignment)

- [x] SMT status semantics explicit modeling.
  - `ModelGuard::StateIn` now uses symbolic status membership (not non-empty shortcuts).
- [x] List semantics alignment with explicit approximation disclosure.
  - `ModelGuard::ListContains` now uses exact bounded slot semantics in SMT.
  - Conflicting `ListContains` guards are rejected under tight bounds (e.g. max list size = 1).
- [x] Shared guard/effect semantics adapter.
  - `crates/temper-verify/src/model/semantics.rs` is now the shared concrete semantics source.
  - Stateright and SMT paths consume shared guard traversal/evaluation utilities.

### Phase 3 (Crate Boundary Cleanup)

- [x] `temper-server` OData focused modules.
  - Completed with:
    - `odata/read.rs`
    - `odata/write.rs`
    - `odata/bindings.rs`
    - `odata/response.rs`
- [x] `temper-mcp` monolith split.
  - Completed with:
    - `protocol.rs` (JSON-RPC framing)
    - `runtime.rs` (stdio runtime loop + sandbox execution orchestration)
    - `tools.rs` (Temper method dispatch + HTTP bridge)
    - `convert.rs` (Monty/JSON conversion)
    - `sandbox.rs` and `spec_loader.rs` retained as focused support modules
- [x] `temper-platform` test harness consolidation.
  - Completed with shared fixtures in `crates/temper-platform/tests/common/`.
  - High-duplication setup moved to shared helpers (`http`, `platform`, `specs`, `dst`).

### Post-Refactor Snapshot (selected files)

- `crates/temper-server/src/odata/write.rs`: **484 LOC** (was 617 after initial split)
- `crates/temper-mcp/src/lib.rs`: **37 LOC** (runtime/tool internals extracted)
- `crates/temper-platform/tests/system_entity_dst.rs`: **736 LOC** (was 1057)
