# Local test performance

Run the full suite with:

```sh
cargo test --workspace --no-fail-fast
```

`cargo nextest run --workspace --no-fail-fast` (what CI uses) runs the same tests
in parallel across test executables, one process per test, and is several times
faster on a many-core machine. It skips doctests; run `cargo test --doc
--workspace` for those.

The development profile (also inherited by tests) keeps line-number backtraces
but omits full variable/type debug information. Incremental compilation and
debug assertions remain enabled. Dependencies carry no debug information at all.
If you need full debugger inspection of workspace code, set
`CARGO_PROFILE_DEV_DEBUG=2 CARGO_PROFILE_TEST_DEBUG=2` (add
`--config 'profile.dev.package."*".debug=2'` for dependencies); switching profiles
requires rebuilding the affected artifacts and uses substantially more disk.

Dependencies are optimized even in development builds (`opt-level = 2`, with
Cranelift and regalloc2 at 3). Cranelift compiles WebAssembly **while tests run**,
and the DST suites also spend much of their time in WASM validation, Cedar, TOML
parsing and the SQL store; leaving that code unoptimized makes app installation
and simulated restarts unnecessarily expensive. Dependencies build once and stay
cached, so the cost is a few extra minutes on a clean build. Application code
remains unoptimized, and release/dist profiles are unchanged.

## Compiled-code reuse in tests

The server and platform enable `temper-wasm/test-shared-compilation` through their
dev-dependencies. WASM unit tests also exercise this path. Each test executable
can reuse up to 128 compiled/pre-linked modules across fresh engine handles.
Concurrent requests for the same module compile it once. There is no disk cache,
unsafe deserialization, cross-process cache, or new external service.

Only immutable code is shared. Every engine handle starts with an empty module
registration map: recovery must still load and register the correct module bytes.
Every invocation creates a new Store, memory/globals, host state and resource
budgets. Host capabilities, secrets and streams come from that invocation's caller,
not from the compiled-code cache. Eviction from the shared code cache does not
revoke an active registration; explicit engine eviction still prevents invocation.

Keep this feature on dev-dependencies. Ordinary production engines still compile
privately, and explicit WASM profiling bypasses the shared test cache. Regression
tests cover cold production engines, reuse, concurrent compilation, bounded
eviction, registration isolation, guest memory/globals and host-secret isolation.

Full randomized workload seed counts, fault injection and invariants are unchanged.
`TEMPER_DST_RANDOM_MODE=smoke` remains an explicitly narrower check, not a
replacement for full coverage.

## Outbound HTTP clients

Each `WasmEngine` owns an `HttpClientCache`, and every production host built for
an invocation clones its HTTP clients from it. Building a `reqwest::Client` loads
and parses the system trust store through OpenSSL, so building one per invocation
cost ~100 ms per WASM callback and contended on OpenSSL's process-wide locks; the
callback-budget integration tests timed out under that load. The cache is keyed by
timeout and `ca_cert:*` secrets and bounded to 64 configurations. It belongs to the
engine rather than the process because pooled connections belong to the tokio
runtime that opened them, and each `#[tokio::test]` runs its own runtime.

## Comparing runs

Separate build time from test time:

```sh
cargo test --workspace --no-fail-fast --no-run --timings
time cargo test --workspace --no-fail-fast
```

Use the same toolchain, features and fixtures on both revisions. CI builds five
GEPA fixtures before running the tests; the pinned toolchain needs the WASM target
first:

```sh
rustup target add wasm32-unknown-unknown
for module in gepa-replay gepa-reflective gepa-score gepa-pareto gepa-verify; do
  cargo build --manifest-path "wasm-modules/$module/Cargo.toml" \
    --target wasm32-unknown-unknown --release
done
```

The shared code cache lasts only for a test process; an unchanged second
`cargo test` does not inherit compiled WASM from the previous process.
