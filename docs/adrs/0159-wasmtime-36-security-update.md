# ADR-0159: Update Wasmtime 29 → 36 (RUSTSEC-2026-0096)

- Status: Accepted
- Date: 2026-08-14
- Issue: ARN-169
- Related: `crates/temper-wasm` (the only crate that depends on Wasmtime)

## Context

The workspace pinned `wasmtime = "29"` / `wasmtime-wasi = "29"`. Wasmtime 29 is
affected by **RUSTSEC-2026-0096** (CVSS 9.0). Temper compiles and runs untrusted
guest WASM modules (`temper-wasm`), so a vulnerability in the WASM runtime is
directly reachable by guest input and must be closed.

## Decision

Update to `wasmtime = "36.0.13"` / `wasmtime-wasi = "36.0.13"` — a maintained 36.x
train that carries the fix (36.0.7 patches RUSTSEC-2026-0096), chosen over the
newest (46.x) to minimize API churn. The floor is `36.0.13` specifically, not
`36.0.12`, because 36.0.13 also fixes RUSTSEC-2026-0222, so an earlier 36.x patch
must not be admitted. `cargo audit` confirms zero wasmtime advisories on the
resolved graph, and RUSTSEC-2026-0096 present on `main` / gone on this branch.

The one import the API move requires:
`wasmtime_wasi::pipe::MemoryOutputPipe` → `wasmtime_wasi::p2::pipe::MemoryOutputPipe`.

**Pin the WASM feature surface so the bump does not widen the sandbox.** wasmtime
30+ turns several proposals on by default that 29 rejected; inheriting the 36
defaults would silently expand what a guest can do relative to the reviewed 29
surface. The engine now sets these explicitly:
- `wasm_memory64(false)` — 29 rejected 64-bit memories at compile, and memory64 is
  a prerequisite for RUSTSEC-2026-0096; keep it rejected.
- `wasm_threads(false)` — rejects shared memories, which a guest could otherwise
  grow outside the per-memory `max_memory` limiter.
- `wasm_multi_memory(false)` — the `max_memory` limiter caps each memory
  individually; a single memory per module keeps the per-invocation budget meaningful.
- `MemoryLimiter` now bounds table and memory host allocation on two axes:
  `table_growing` denies growth past `MAX_TABLE_ELEMENTS` (1,000,000) per table,
  and `tables()`/`memories()` cap the store's table/memory *counts* (`MAX_TABLES`
  = 8, `MAX_MEMORIES` = 1). wasmtime's default limiter allows 10,000 of each, so
  the per-table element cap alone was not a store-wide budget — a guest could
  declare thousands of tables at the cap. Together these bound total table host
  memory to `MAX_TABLES * MAX_TABLE_ELEMENTS`.

Temper's guests are single-memory wasm32 modules and use none of the disabled
features; the `temper-wasm` engine suite passes with the surface pinned.

## Consequences

- RUSTSEC-2026-0096 (and RUSTSEC-2026-0222, via the 36.0.13 floor) is closed for
  the guest-WASM execution path.
- The reviewed 29 sandbox surface is *preserved*, not merely inherited: memory64,
  shared memory, and multiple memories are rejected; the linear-memory limiter and
  the new table cap deny growth past budget; fuel/epoch timeouts fire; traps stay
  isolated. Regression tests cover each: `memory_growth_denied_by_limiter` (now
  non-vacuous — the guest traps if the grow unexpectedly succeeds),
  `initial_memory_over_budget_is_rejected`, `table_growth_denied_past_cap`,
  `too_many_tables_is_rejected`, `memory64_module_is_rejected`,
  `multi_memory_module_is_rejected`, `shared_memory_module_is_rejected`, and
  `wasi_preview1_module_invokes_end_to_end`.

## Follow-ups (pre-existing, tracked separately)

- **WASI host-side allocation in preview1 host calls.** `random_get` (and peers)
  allocate a host buffer sized by the guest-supplied length before the guest-memory
  bounds check; the `ResourceLimiter` governs guest memory/tables, not host-side
  allocations inside host functions, so a guest can force a large transient host
  allocation. This predates the bump (present on 29) and its clean fix needs either
  a wasmtime train with the embedder allocation controls (42+) or a custom WASI
  host wrapper — a separate hardening effort, not this security patch.

## Alternatives considered

- **Jump straight to 46.x (latest).** Rejected for now: a larger API delta for no
  additional security benefit over 36.x for this advisory. A later routine bump can
  move further once the surface is re-reviewed.
- **Backport a patch onto 29.** Not offered upstream; the fix ships in the newer
  release trains.
