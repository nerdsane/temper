# Local test performance

Run the full suite with:

```sh
cargo test --workspace --no-fail-fast
```

For shorter wall-clock runs across independent test binaries, install
[cargo-nextest](https://nexte.st/docs/installation/pre-built-binaries/) and run:

```sh
cargo nextest run --workspace --no-fail-fast --retries 0
cargo test --workspace --doc
```

The second command preserves doctest coverage, which nextest does not run.
Keep the default full randomized workload for comparable coverage. Nextest uses
separate test processes, so the in-process WASM compilation cache described below
does not carry across its test cases. Measure both runners on your hardware;
the runner change does not make compilation itself faster.

Some existing PostgreSQL fixtures retain Docker containers and volumes after a
local run, including with plain Cargo. More test processes can leave more
containers. Clean up only resources created by your run, not unrelated containers
or shared images.

The development profile (also inherited by tests) keeps line-number backtraces
for workspace code but omits full variable/type debug information. Third-party
dependencies retain symbol names but omit debug sections, which otherwise get
copied into every integration-test executable. Incremental compilation, debug
assertions and overflow checks remain enabled.

For full debugger inspection, including dependencies, use:

```sh
CARGO_PROFILE_DEV_DEBUG=2 CARGO_PROFILE_TEST_DEBUG=2 \
  cargo --config 'profile.dev.package."*".debug=2' test --workspace
```

Switching these settings requires rebuilding affected artifacts and uses more
disk. Release/dist profiles are unchanged.

Cranelift and regalloc2 are optimized even in development builds. These crates
compile WebAssembly **while tests run**; leaving the compiler itself unoptimized
makes app installation and simulated restarts unnecessarily expensive. Application
code remains unoptimized, and release/dist profiles are unchanged.

TOML, TOML-edit, quick-xml and SHA-2 use optimization level 1: parsing and hashing
are hot paths in full-seed simulations. This is a targeted override, not a blanket
optimization of third-party dependencies. Level 1 preserves
[cross-crate generic code sharing](https://doc.rust-lang.org/cargo/reference/profiles.html#overrides-and-generics);
debug assertions and overflow checks remain enabled. Measure clean builds,
edit/rebuilds and test execution separately when changing these overrides.

## Storage compilation boundary

The Turso adapter gives its SQL futures explicit `Send` bounds and keeps embedded
engine handles behind private, compiler-checked `Send + Sync` interfaces. This
prevents each downstream async-trait implementation from repeatedly checking the
embedded engine's large internal type graph. It does not change the SQL, retries,
transaction lifetime, cancellation, or when a future starts doing work.

The boundary uses no unsafe trait implementations. It adds one allocation per
database/native handle/result stream, not per result row; the adapter does not
box returned futures. Driver regression tests cover unpolled futures, row
consumption, contention, commit/rollback, and remote connection cleanup.

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

## HTTP transport reuse

Production invocation hosts reuse their engine's HTTP clients, avoiding repeated
TLS trust-store loading and allowing connections to stay pooled. This applies to
action-triggered integrations, direct invocation and WASM-backed HTTP endpoints.
It is independent of the test-only compiled-code cache and also benefits normal
production engines.

The bounded 64-configuration LRU separates clients by timeout and exact private
CA set. Public and internal clients retain separate pools; internal calls still
disable redirects and proxies. Secrets, capability issuers, authorization wrappers,
request headers and stream registries remain per invocation. Different engine
handles never share these pools, even when they share compiled WASM for tests.
Concurrent misses initialize one client pair per retained configuration.

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
rustup target add wasm32-unknown-unknown
for module in gepa-replay gepa-reflective gepa-score gepa-pareto gepa-verify; do
  cargo build --manifest-path "wasm-modules/$module/Cargo.toml" \
    --target wasm32-unknown-unknown --release
done
```

The shared code cache lasts only for a test process; an unchanged second
`cargo test` does not inherit compiled WASM from the previous process.
