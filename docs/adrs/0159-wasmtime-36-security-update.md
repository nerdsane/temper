# ADR-0159: Update Wasmtime 29 → 36.0.12 (RUSTSEC-2026-0096 sandbox escape)

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
`temper-wasm`, including on aarch64 hosts. Upstream exploitation additionally
depends on memory64 and non-default Spectre/trap settings that Temper does not
enable today, so the vulnerable compiler path is security-boundary-relevant but
not known to be directly exploitable with Temper's current defaults.

The remaining wasmtime advisories are lower severity but real: heap OOB reads and
panics in the component-model string transcoders, WASI resource exhaustion, pooling-
allocator cross-instance data leakage, Winch mis-masked `table.grow`/`table.fill`,
and WASIp1 `fd_renumber` / `path_open` / hard-link `FilePerms` bypasses. All of them
are fixed in wasmtime 30–36; none has a backport to the 29.x line.

## Decision

Pin the workspace to **wasmtime / wasmtime-wasi 36.0.12**, the maintained LTS
line containing every relevant backport while remaining below Temper's Rust
1.92 MSRV. Wasmtime 36.0.12 declares `rust-version = "1.86.0"`; Wasmtime
46.0.1 declares `rust-version = "1.94.0"`.

This choice is driven by the complete current advisory set, not just the
original April finding. The critical aarch64 fix was backported to 36.0.7, the
May WASIp1 `path_open(TRUNCATE)` fix to 36.0.10, the June `fd_renumber` leak to
36.0.11, and the June hard-link/rename `FilePerms` bypass
(RUSTSEC-2026-0188) to 36.0.12. The newest Rust-1.92-compatible 44.x release,
44.0.3, remains affected by RUSTSEC-2026-0188; the fixed 45/46 lines require a
higher Rust MSRV. Version 36.0.12 is therefore the newest fully patched choice
that does not silently break Temper's supported toolchain.

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
`set_fuel`/`set_epoch_deadline`) is unchanged from 29 to 36. All of
`host_functions.rs` and `telemetry.rs` compile untouched.

The only break is the `wasmtime-wasi` module reorganization that moved the
shared pipe implementation under the Preview 2 module. Migrated in
`engine/mod.rs`:

| 29.x | 36.x |
| --- | --- |
| `wasmtime_wasi::preview1::WasiP1Ctx` | `wasmtime_wasi::preview1::WasiP1Ctx` (unchanged) |
| `wasmtime_wasi::preview1::add_to_linker_sync` | `wasmtime_wasi::preview1::add_to_linker_sync` (unchanged) |
| `wasmtime_wasi::pipe::MemoryOutputPipe` | `wasmtime_wasi::p2::pipe::MemoryOutputPipe` |
| `wasmtime_wasi::WasiCtxBuilder` | `wasmtime_wasi::WasiCtxBuilder` (unchanged) |
| `WasiCtxBuilder::build_p1()` | `WasiCtxBuilder::build_p1()` (unchanged) |

`add_to_linker_sync`'s accessor closure bound (`impl Fn(&mut T) -> &mut WasiP1Ctx +
Copy + Send + Sync + 'static`) is unchanged, so the existing accessor compiles as-is.

## Consequences

### Positive
- RUSTSEC-2026-0096 and the other Wasmtime/Wasmtime-WASI advisories affecting
  29.0.1 clear on an upstream-supported release line.
- Temper keeps its documented Rust 1.92 support and its existing Docker build.
- The dependency movement stays within the security runtime family instead of
  mixing a Rust-platform migration into an emergency sandbox fix.

### Negative
- A seven-major-version jump pulls newer Cranelift/regalloc transitively and still
  produces a substantial `Cargo.lock` diff. The lockfile must be generated with
  targeted updates so unrelated packages do not move opportunistically.
- This deliberately does not take features introduced only in newer Wasmtime lines.
  Temper does not use them, and they are not required to close this issue.

### DST Compliance
`temper-wasm` is not a simulation-visible crate (it is not in temper-runtime /
temper-jit / temper-server's deterministic core; WASM invocation already runs on a
dedicated OS thread behind `// determinism-ok` boundaries). No sim-core code
changes. No new `// determinism-ok` annotations needed.

## Non-Goals

- The temperpaw side of ARN-169 (its own `wasmtime` pin) — separate repo, separate PR.
- Adopting the wasmtime component-model (`p2`/`p3`) host API — the guest ABI stays
  the custom `env.*` core-wasm linker plus WASIp1; unchanged here.
- Opportunistic lockfile-only updates outside the Wasmtime/WASI dependency
  closure. Those changes need their own review and must not ride this CVE fix.
- Advisories left standing because no safe bump exists in this PR (tracked as
  follow-ups, not decided here): `quick-xml` 0.37.5 (RUSTSEC-2026-0194/0195 — a
  breaking 0.37 → 0.41 Reader/Error migration in `temper-spec`'s CSDL parser, not a
  lockfile bump); a **second** `rustls-webpki` 0.102.8 copy pinned by
  `libsql → hyper-rustls → rustls 0.22` in the turso-store path (needs libsql on
  rustls 0.23); `protobuf` 2.28.0 pinned by `pprof`; and `rsa` / `tokio-tar` which
  have no fixed release. This PR does not claim a clean `cargo audit`; it requires
  the Wasmtime/Wasmtime-WASI advisory class to be fully cleared and reports the
  remaining unrelated advisories from the final resolved graph.

## Alternatives Considered

1. **Use Wasmtime 46.0.1 and raise Temper's MSRV to 1.94.** Rejected: this
   emergency dependency fix is not authorization for a repository-wide Rust
   platform migration, and the current PR did not update or test the 1.92
   Docker/SDK/reference-app contract.
2. **Use Wasmtime 44.0.3.** Rejected after the current advisory audit:
   RUSTSEC-2026-0188 still affects this version and has no 44.x backport.
3. **Backport-only / stay on 29.x.** Rejected: no 29.x backport exists for
   RUSTSEC-2026-0096; staying leaves a CVSS 9.0 sandbox escape open.

## Rollback Policy

Revert the `Cargo.toml` pins and `engine/mod.rs` WASI path changes, then
`cargo update -p wasmtime -p wasmtime-wasi --precise 29.0.1`. The change is
self-contained to `temper-wasm` + the workspace lockfile, so rollback is a single
revert commit — but it reopens RUSTSEC-2026-0096, so rollback is a last resort.
