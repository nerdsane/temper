# Local test performance

Run the full suite with:

```sh
cargo test --workspace --no-fail-fast
```

The development profile (also inherited by tests) keeps line-number backtraces
but omits full variable/type debug information. Incremental compilation and
debug assertions remain enabled. If you need full debugger inspection, set
`CARGO_PROFILE_DEV_DEBUG=2 CARGO_PROFILE_TEST_DEBUG=2`; switching profiles
requires rebuilding the affected artifacts and uses substantially more disk.

Cranelift and regalloc2 are optimized even in development builds. These crates
compile WebAssembly **while tests run**; leaving the compiler itself unoptimized
makes app installation and simulated restarts unnecessarily expensive. Application
code remains unoptimized, and release/dist profiles are unchanged.

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

The callback-budget integration cases reserve one test slot before starting their
wall-clock deadlines. This prevents the independent 240-callback load from starving
the shorter deadline cases; all callback counts, deadlines and assertions remain
unchanged. Other test executables retain their normal parallelism.

## Comparing runs

Separate build time from test time:

```sh
cargo test --workspace --no-fail-fast --no-run --timings
time cargo test --workspace --no-fail-fast
```

Use the same toolchain, features and fixtures on both revisions. CI builds five
GEPA fixtures before running the tests:

```sh
for module in gepa-replay gepa-reflective gepa-score gepa-pareto gepa-verify; do
  cargo build --manifest-path "wasm-modules/$module/Cargo.toml" \
    --target wasm32-unknown-unknown --release
done
```

The shared code cache lasts only for a test process; an unchanged second
`cargo test` does not inherit compiled WASM from the previous process.
