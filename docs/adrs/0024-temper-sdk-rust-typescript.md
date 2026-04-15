# ADR-0024: Temper SDK — Rust and TypeScript Client Libraries

- Status: Accepted
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0006: Spec-aware agent interface for MCP
  - ADR-0020: Temper agent CLI command
  - `crates/temper-cli/src/agent/mod.rs` (inline TemperClient to extract)
  - `crates/temper-mcp/` (dispatch surface the SDK mirrors)
  - `packages/temper-sdk-ts/` (TypeScript SDK home)

## Context

The `temper agent` CLI command (ADR-0020) contains an inline `TemperClient` struct that wraps HTTP calls to the Temper server. This client is useful beyond the CLI — any Rust code that interacts with Temper entities needs the same HTTP surface. Similarly, the TypeScript SDK (`packages/temper-sdk-ts/`) needs a general-purpose client for entity CRUD, governance, and SSE event streaming.

Duplicating HTTP client logic across consumers leads to drift and maintenance burden. A thin, dedicated SDK in both Rust and TypeScript gives all consumers a single, tested client.

## Decision

### Sub-Decision 1: Thin HTTP Client, Not a Framework

The SDK is a thin HTTP client that mirrors the temper-mcp dispatch surface. It does not embed business logic, state machines, or agent loops. It wraps:

- Entity CRUD: list, get, create, patch, action
- Governance: authorize, audit, get_decisions
- Spec management: submit_specs
- SSE: event streaming

**Why this approach**: The server is the source of truth. The SDK is a transport layer. Keeping it thin means it stays stable as server-side behavior evolves.

### Sub-Decision 2: Rust SDK (`temper-sdk` Crate)

A new `crates/temper-sdk/` workspace member extracted from the inline `TemperClient` in `temper-cli`. Uses `reqwest` for HTTP, `futures-util` for SSE streaming, builder pattern for configuration.

The CLI's inline client is replaced with `use temper_sdk::TemperClient`.

**Why this approach**: Extraction, not invention. The API surface already works in production via `temper agent`. The SDK just makes it reusable.

### Sub-Decision 3: TypeScript SDK in temper-sdk-ts

A new `TemperClient` class in `packages/temper-sdk-ts/src/client.ts`, re-exported from `index.ts`. Uses `fetch` (no extra dependencies). Includes `AsyncGenerator`-based SSE support via `watchEvents()`.

**Why this approach**: A standalone `@temper/sdk` package keeps the SDK independent of any specific agent framework. It has zero runtime dependencies — just `fetch`.

### Sub-Decision 4: SSE Subscription Support

Both SDKs include SSE event streaming for real-time entity change notifications. Rust uses `reqwest` streaming response with line-by-line SSE parsing. TypeScript uses `fetch` with `ReadableStream` and async generator.

**Why this approach**: Agents and UIs need to react to entity state changes without polling.

### Sub-Decision 5: Builder Pattern for Configuration

Rust uses `ClientBuilder` with `base_url()`, `tenant()`, `principal()` methods. TypeScript uses a `TemperClientConfig` interface passed to the constructor.

**Why this approach**: Builder pattern is idiomatic Rust and allows future configuration options without breaking changes.

## Rollout Plan

1. **Phase 0 (This PR)** — Create `temper-sdk` crate, TypeScript client, refactor CLI to use SDK, add tests.
2. **Phase 1 (Follow-up)** — Add retry/backoff, connection pooling configuration, auth token support.

## Consequences

### Positive
- Single source of truth for Temper HTTP client logic in each language.
- CLI agent code becomes simpler (no inline HTTP client).
- External consumers (agent executor, other tools) can depend on `temper-sdk`.
- SSE support enables reactive patterns without polling.

### Negative
- One more crate to maintain in the workspace.
- CLI gains an internal dependency (but it already depends on many workspace crates).

### Risks
- Server API changes require SDK updates. Mitigated: the SDK is workspace-internal and versioned together.

## Non-Goals

- Authentication/token management (handled by server middleware).
- Offline caching or client-side state machines.
- Code generation from OpenAPI specs.

## Alternatives Considered

1. **Generate SDK from OpenAPI spec** — The server does not currently emit an OpenAPI spec. Building one just for SDK generation is premature.
2. **Keep client inline in CLI** — Works for one consumer but forces duplication when the agent executor binary needs the same client.
3. **Keep TypeScript SDK in temper-pi** — temper-pi was removed as the Pi framework is no longer used. A standalone `@temper/sdk` package is cleaner.
