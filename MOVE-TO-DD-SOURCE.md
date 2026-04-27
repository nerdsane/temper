# Moving Temper to dd-source — Layer-by-Layer Plan

> **Target**: `~/dd/dd-source/domains/odp/temper/`
> **Source**: `~/go/src/github.com/DataDog/temper/` (DataDog/temper GitHub repo)
> **Precedent**: `domains/odp/apps/gumshoe/` — Rust workspace already in dd-source

## 1. Current State

### Temper workspace: 24 crates, ~67k lines of Rust

```
LEAF CRATES (0 internal deps):              Lines
  temper-macros          proc macros            30 :done:
  temper-runtime         core types + traits  3532 :done:
  temper-spec            IOA/CSDL specs       5254 :done:
  temper-wasm-sdk        WASM guest SDK        442

LAYER 1 (depend only on leaves):
  temper-agentos         agent OS runtime     5268  → runtime
  temper-authz           Cedar policies        946  → runtime :done:
  temper-codegen         code generation        629  → spec
  temper-jit             JIT compiler         1260  → runtime, spec
  temper-observe         observability        1614  → runtime
  temper-odata           OData API            1707  → runtime
  temper-store-postgres  Postgres store        994  → runtime
  temper-store-redis     Redis store          1615  → runtime
  temper-store-sim       Sim/test store        498  → runtime
  temper-store-turso     Turso/SQLite store   1305  → runtime
  temper-verify          formal verification  4204  → runtime, spec

LAYER 2+ (depend on layer 1):
  temper-evolution       evolution engine     1868  → observe, runtime, spec
  temper-mcp             MCP integration      2753  → spec, runtime, server
  temper-optimize        optimizer            1049  → jit, observe, runtime
  temper-wasm            WASM host runtime    1046  → authz
  temper-server          HTTP server         25845  → (almost everything)
  temper-platform        E2E platform         3466  → (almost everything)
  temper-cli             CLI binary           3512  → (everything)

REFERENCE APPS (not moving):
  ecommerce-reference
  oncall-reference

NON-RUST (not moving initially):
  packages/temper-pi     TypeScript SDK
  wasm-modules/          WASM test fixtures
  reference-apps/        demo apps
```

### dd-source ODP today

```
domains/odp/
├── apps/
│   ├── apis/
│   │   └── lassie-ng/        ← Go, Rapid framework
│   ├── gumshoe/               ← Rust workspace (precedent!)
│   ├── coscientist/
│   └── bitsevolve/
├── libs/
│   └── mcp-client/
├── shared/
├── scripts/
└── Tiltfile
```

Gumshoe is the precedent: Rust workspace with `Cargo.toml` + `BUILD.bazel` at
`domains/odp/apps/gumshoe/`. So dd-source already supports Rust workspaces in ODP.

## 2. Target Structure

```
domains/odp/temper/
├── Cargo.toml                  ← workspace root
├── BUILD.bazel                 ← Bazel build (follows gumshoe pattern)
├── rust-toolchain.toml         ← nightly-2026-02-08, edition 2024
├── TEMPER-LASSIE.md            ← integration design doc
│
├── crates/
│   ├── temper-macros/
│   ├── temper-runtime/
│   ├── temper-spec/
│   ├── temper-codegen/
│   ├── temper-authz/
│   ├── temper-observe/
│   ├── temper-odata/
│   ├── temper-jit/
│   ├── temper-verify/
│   ├── temper-store-postgres/
│   ├── temper-store-redis/
│   ├── temper-store-sim/
│   ├── temper-store-turso/
│   ├── temper-wasm/
│   ├── temper-wasm-sdk/
│   ├── temper-agentos/
│   ├── temper-evolution/
│   ├── temper-mcp/
│   ├── temper-optimize/
│   ├── temper-server/
│   ├── temper-platform/
│   └── temper-cli/
│
└── docker-compose.yml          ← local dev (Postgres, Redis, ClickHouse, OTEL)
```

**What does NOT move** (stays in DataDog/temper or is dropped):
- `reference-apps/` — demo apps, not needed in dd-source
- `packages/temper-pi/` — TypeScript SDK, separate lifecycle
- `wasm-modules/` — test fixtures, can be inline or fetched
- `.github/` — GitHub Actions CI (dd-source uses its own CI)
- `NOTES*.md`, `ACTOR-LLM.md` — scratch files

## 3. Strategy: Copy, Not Fork-Sync

**Don't** set up fork syncing or git subtree. The DataDog/temper repo is a development
sandbox. Once crates land in dd-source, dd-source is the source of truth. The external
repo can be archived or kept as read-only reference.

Rationale:
- Fork syncing creates merge hell for a fast-moving codebase
- dd-source has its own CI, Bazel build, and deployment pipeline
- The crate boundaries are clean — we can copy layer by layer and verify each

## 4. Layer-by-Layer Move

Each layer is a separate PR into dd-source. Each PR must:
1. Copy crate source
2. Add/update `Cargo.toml` workspace members
3. Add `BUILD.bazel` if required by dd-source CI
4. Verify `cargo build` and `cargo test` pass
5. Get review + merge before starting next layer

### Layer 0: Workspace scaffold (no crates yet)

**PR 1: Create the workspace**

```bash
mkdir -p domains/odp/temper/crates
```

Create:
- `domains/odp/temper/Cargo.toml` — empty workspace, `resolver = "2"`, edition 2024
- `domains/odp/temper/rust-toolchain.toml` — `nightly-2026-02-08`
- `domains/odp/temper/BUILD.bazel` — follow gumshoe pattern
- `domains/odp/temper/TEMPER-LASSIE.md` — integration design doc

Verify: `cargo check --workspace` succeeds (empty workspace).

### Layer 1: Leaf crates (0 internal deps)

**PR 2: `temper-macros`, `temper-runtime`, `temper-spec`, `temper-wasm-sdk`**

These have zero internal dependencies. Copy as-is.

```
temper-macros    (30 lines)   — proc macros
temper-runtime   (3532 lines) — EntityActor, TransitionEvent, TenantId, core traits
temper-spec      (5254 lines) — IOA spec parser, CSDL, Cedar policy types
temper-wasm-sdk  (442 lines)  — WASM guest-side SDK
```

Verify: `cargo test -p temper-macros -p temper-runtime -p temper-spec -p temper-wasm-sdk`

### Layer 2: Core infrastructure (depend only on leaves)

**PR 3: Stores + authz + observe**

```
temper-authz           (946 lines)  → runtime
temper-observe         (1614 lines) → runtime
temper-store-postgres  (994 lines)  → runtime
temper-store-redis     (1615 lines) → runtime
temper-store-sim       (498 lines)  → runtime
temper-store-turso     (1305 lines) → runtime
```

These are the "infrastructure" crates. No business logic, just storage backends,
Cedar authorization, and OpenTelemetry integration.

Verify: `cargo test` for all 6 crates. Stores need Postgres/Redis for integration
tests (or `--lib` for unit tests only).

**PR 4: Codegen + JIT + Verify + OData**

```
temper-codegen  (629 lines)  → spec
temper-jit      (1260 lines) → runtime, spec
temper-verify   (4204 lines) → runtime, spec
temper-odata    (1707 lines) → runtime
```

These are the "spec processing" crates. Parse specs, generate code, verify invariants,
expose OData API.

Verify: `cargo test` for all 4 crates. Verify tests use stateright/proptest
(in-process, no external deps).

### Layer 3: AgentOS + higher-level crates

**PR 5: `temper-agentos`**

```
temper-agentos  (5268 lines) → runtime
```

The agent OS runtime — Process entity, ProcessScheduler, IOA state machine, drivers.
This is the core of the CRI story.

Verify: `cargo test -p temper-agentos`

**PR 6: `temper-evolution`, `temper-optimize`, `temper-wasm`**

```
temper-evolution  (1868 lines) → observe, runtime, spec
temper-optimize   (1049 lines) → jit, observe, runtime
temper-wasm       (1046 lines) → authz
```

Verify: `cargo test` for all 3.

### Layer 4: Server + MCP + Platform

**PR 7: `temper-server`**

```
temper-server  (25845 lines) → (almost everything)
```

The big one — HTTP server, all entity handlers, multi-tenant, integration tests.
Depends on most crates from layers 0-3.

Verify: `cargo test -p temper-server` (includes integration tests with Postgres/Redis).

**PR 8: `temper-mcp`, `temper-platform`**

```
temper-mcp       (2753 lines) → spec, runtime, server
temper-platform   (3466 lines) → (almost everything)
```

MCP integration and E2E platform tests (including DST).

Verify: `cargo test -p temper-mcp -p temper-platform`

### Layer 5: CLI binary

**PR 9: `temper-cli`**

```
temper-cli  (3512 lines) → (everything)
```

The CLI binary. Depends on all crates. This is the final "everything compiles" check.

Verify: `cargo build -p temper-cli && cargo test -p temper-cli`

### Layer 6: Docs + config + infra

**PR 10: Docker, docs, CI integration**

- `docker-compose.yml` for local dev
- CI pipeline configuration (Bazel / dd-source CI)
- Any remaining config files

## 5. What to Watch Out For

### Bazel
dd-source uses Bazel. Gumshoe has `BUILD.bazel` files — follow the same pattern.
May need `rules_rust` setup or existing dd-source Rust toolchain config.

### Rust toolchain
Temper requires `nightly-2026-02-08` (Edition 2024, rust-version 1.92). Verify
dd-source CI supports this or can be configured to.

### External dependencies
Some deps may need vetting for dd-source:
- `wasmtime` (29.x) — WASM runtime, large dep tree
- `stateright` (0.31) — model checker, dev-only
- `cedar-policy` (4.x) — Amazon's Cedar, production dep

### Database migrations
Temper has `sqlx` migrations in `temper-store-postgres`. These need to coexist with
lassie-ng's OrgStore Postgres. May need separate database or schema prefix.

### Test infra
Some tests need running services (Postgres, Redis). dd-source CI may have these
available or may need `docker-compose` in the test pipeline.

### Path references
All `path = "crates/temper-*"` in workspace Cargo.toml should just work since we're
preserving the same relative structure.

## 6. Rollback

Each layer is an independent PR. If a layer breaks, revert that PR. Earlier layers
are unaffected. No cascading failures.

## 7. Timeline Estimate

| Layer | PR | Crates | Effort | Blocker |
|---|---|---|---|---|
| 0 | PR 1 | scaffold | 1 day | Bazel setup, Rust toolchain in CI |
| 1 | PR 2 | 4 leaves | 1 day | — |
| 2 | PR 3-4 | 10 infra | 2 days | DB in CI for store tests |
| 3 | PR 5-6 | 4 mid-tier | 1 day | — |
| 4 | PR 7-8 | 3 server | 2 days | Full integration test infra |
| 5 | PR 9 | 1 CLI | 1 day | — |
| 6 | PR 10 | docs/CI | 1 day | CI pipeline review |
| | | **Total** | **~9 days** | |

## 8. Open Questions

- [ ] Does dd-source CI support `nightly-2026-02-08`? Or do we need to request toolchain?
- [ ] Bazel: can we follow gumshoe's `BUILD.bazel` pattern exactly, or does Temper's
      larger dep tree need custom rules?
- [ ] Database: separate Postgres database for Temper, or shared with lassie-ng OrgStore?
- [ ] Should we keep DataDog/temper as read-only archive, or delete?
- [ ] Do we move TypeScript SDK (`packages/temper-pi`) separately, or not at all?
- [ ] WASM test fixtures (`wasm-modules/`) — inline in test crate, or fetch at test time?
