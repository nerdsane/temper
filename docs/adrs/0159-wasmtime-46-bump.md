# ADR-0159: Bump wasmtime 29 → 46 (RUSTSEC-2026-0096 sandbox escape)

- Status: Accepted
- Date: 2026-07-07
- Deciders: Temper core maintainers
- Related:
  - ARN-169 (security): the tracking issue
  - `crates/temper-wasm/src/engine/mod.rs` (WASI + engine setup — where the API moved)
  - `crates/temper-wasm/src/engine/host_functions.rs`, `telemetry.rs` (core wasmtime API — unchanged)
  - `Cargo.toml` (workspace `wasmtime` / `wasmtime-wasi` pins)

## Context

The workspace pinned `wasmtime = "29"` / `wasmtime-wasi = "29"` (resolved 29.0.1).
`cargo audit` flagged 16 `wasmtime` + 3 `wasmtime-wasi` advisories against that
version. The most severe is **RUSTSEC-2026-0096 (CVSS 9.0)**: an aarch64 Cranelift
miscompilation of guest heap accesses that lets a WASM guest read/write outside its
linear-memory sandbox — a full sandbox escape and host compromise. The kernel
compiles and runs untrusted integration modules as WASM guests through
`temper-wasm`, and Temper's macOS/ARM dev machines and any aarch64 host run exactly
the affected code path, so this is directly reachable.

The remaining wasmtime advisories are lower severity but real: heap OOB reads and
panics in the component-model string transcoders, WASI resource exhaustion, pooling-
allocator cross-instance data leakage, Winch mis-masked `table.grow`/`table.fill`,
and WASIp1 `fd_renumber` / `path_open` / hard-link `FilePerms` bypasses. All of them
are fixed in wasmtime 30–36; none has a backport to the 29.x line.

## Decision

Bump the workspace pin to the current latest, **wasmtime / wasmtime-wasi 46**, rather
than the minimum-viable 36. The API-migration surface is the same either way (the
breaking WASI reorg lands at 34, below both targets), so taking latest clears the
most advisories, tracks upstream's supported line, and avoids re-doing this bump
next quarter. The workspace MSRV (1.92) and toolchain (nightly 1.95) satisfy
wasmtime 46's MSRV.

This is a **security bump, not a behavior change**. Every sandbox control is
preserved unchanged:

- per-invocation fresh `Store` (no state reuse across guests),
- fuel budget (`Config::consume_fuel` + `Store::set_fuel`),
- wall-clock epoch timeout (`Config::epoch_interruption` + shared `EpochTicker` +
  per-store relative deadline),
- memory limiter (`ResourceLimiter::memory_growing` capping `memory.grow`),
- WASI with no preopened dirs, no inherited env, no network — only an in-memory
  stderr pipe (`WasiCtxBuilder::new().build_p1()`).

### What broke (and what did not)

The core embedding API (`Config`, `Engine`, `Module`, `Linker`, `InstancePre`,
`Store`, `ResourceLimiter`, `Caller`, `Memory`, `Trap::OutOfFuel`,
`Trap::Interrupt`, `ProfilingStrategy`, `func_wrap`, `get_typed_func`,
`set_fuel`/`set_epoch_deadline`) is unchanged from 29 to 46. All of
`host_functions.rs` and `telemetry.rs` compile untouched.

The only break is the `wasmtime-wasi` module reorganization (wasmtime 34), which
renamed the preview1/preview2 modules. Migrated in `engine/mod.rs`:

| 29.x | 46.x |
| --- | --- |
| `wasmtime_wasi::preview1::WasiP1Ctx` | `wasmtime_wasi::p1::WasiP1Ctx` |
| `wasmtime_wasi::preview1::add_to_linker_sync` | `wasmtime_wasi::p1::add_to_linker_sync` |
| `wasmtime_wasi::pipe::MemoryOutputPipe` | `wasmtime_wasi::p2::pipe::MemoryOutputPipe` |
| `wasmtime_wasi::WasiCtxBuilder` | `wasmtime_wasi::WasiCtxBuilder` (unchanged) |
| `WasiCtxBuilder::build_p1()` | `WasiCtxBuilder::build_p1()` (unchanged) |

`add_to_linker_sync`'s accessor closure bound (`impl Fn(&mut T) -> &mut WasiP1Ctx +
Copy + Send + Sync + 'static`) is unchanged, so the existing accessor compiles as-is.

## Consequences

### Positive
- RUSTSEC-2026-0096 and the other 18 wasmtime/wasmtime-wasi advisories clear.
- On latest upstream, so security backports land without another major bump.

### Negative
- A ~17-major-version jump pulls newer cranelift/regalloc transitively; larger diff
  in `Cargo.lock`. Mitigated by the WASM test suite (fuel, timeout, memory-limit,
  trap-isolation) passing unchanged.

### DST Compliance
`temper-wasm` is not a simulation-visible crate (it is not in temper-runtime /
temper-jit / temper-server's deterministic core; WASM invocation already runs on a
dedicated OS thread behind `// determinism-ok` boundaries). No sim-core code
changes. No new `// determinism-ok` annotations needed.

## Non-Goals

- The temperpaw side of ARN-169 (its own `wasmtime` pin) — separate repo, separate PR.
- Adopting the wasmtime component-model (`p2`/`p3`) host API — the guest ABI stays
  the custom `env.*` core-wasm linker plus WASIp1; unchanged here.
- Ancillary advisories cleared by lockfile-only bumps in the same PR (not
  architectural decisions): `postgres-protocol` 0.6.12, `tokio-postgres` 0.7.18,
  `quinn-proto` 0.11.16, `crossbeam-epoch` 0.9.20, and the `rustls-webpki` 0.103
  line (0.103.9 → 0.103.13, the reqwest/quinn copy).
- Advisories left standing because no safe bump exists in this PR (tracked as
  follow-ups, not decided here): `quick-xml` 0.37.5 (RUSTSEC-2026-0194/0195 — a
  breaking 0.37 → 0.41 Reader/Error migration in `temper-spec`'s CSDL parser, not a
  lockfile bump); a **second** `rustls-webpki` 0.102.8 copy pinned by
  `libsql → hyper-rustls → rustls 0.22` in the turso-store path (needs libsql on
  rustls 0.23); `protobuf` 2.28.0 pinned by `pprof`; and `rsa` / `tokio-tar` which
  have no fixed release. This PR does not claim a clean `cargo audit` — it takes it
  from 37 to 9, with the wasmtime class fully cleared.

## Alternatives Considered

1. **Pin to the minimum 36.** Rejected: same migration cost (the WASI reorg is at
   34), fewer advisories cleared, and a near-term re-bump.
2. **Backport-only / stay on 29.x.** Rejected: no 29.x backport exists for
   RUSTSEC-2026-0096; staying leaves a CVSS 9.0 sandbox escape open.

## Rollback Policy

Revert the `Cargo.toml` pins and `engine/mod.rs` WASI path changes, then
`cargo update -p wasmtime -p wasmtime-wasi --precise 29.0.1`. The change is
self-contained to `temper-wasm` + the workspace lockfile, so rollback is a single
revert commit — but it reopens RUSTSEC-2026-0096, so rollback is a last resort.
