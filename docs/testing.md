# Local test performance

Run the full suite with:

```sh
cargo test --workspace --no-fail-fast
```

Server and platform integration suites are compiled by the test-only
`temper-integration-tests` workspace package, in two executables named `server`
and `platform`. Their source files remain under the original crates' `tests/`
directories. Consolidating the executables avoids linking the server and its
dependencies once per suite; the integration package also lets server unit tests
compile without waiting for the platform integration harness.

An original suite name is now a module prefix. To run one suite:

```sh
cargo test -p temper-integration-tests --test server odata_read::
cargo test -p temper-integration-tests --test platform platform_e2e_dst::
cargo nextest run -p temper-integration-tests --test server -E 'test(/^dst_platform_random::/)'
```

`cargo test -p temper-server` and `cargo test -p temper-platform` run those
packages' unit tests; add the integration package or use `--workspace` for the
complete coverage. Historical ADR commands with `--test <suite>` predate this
layout. Server suites gated by `observe` use the integration package's default
`observe` feature, which forwards to `temper-server/observe`.

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
copied into every integration-test executable. Debug assertions and overflow
checks remain enabled. Ordinary development builds retain incremental compilation;
the test profile disables it, uses 16 codegen units and disables local ThinLTO
to reduce full-workspace compile time and compiler-cache disk use. This trades away incremental test
rebuild reuse; measure small edits separately from clean builds. To opt back in:

```sh
CARGO_PROFILE_TEST_INCREMENTAL=true CARGO_PROFILE_TEST_CODEGEN_UNITS=256 \
  cargo test --workspace --no-fail-fast
```

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
code in ordinary development builds remains unoptimized, and release/dist profiles
are unchanged. Tests additionally optimize temper-runtime, temper-jit, temper-spec
and the integration harnesses at level 1; debug assertions and overflow checks
remain enabled.

The test profile also optimizes `turso_core` at level 1 for the unchanged SQL
connection-lifetime workload. The two Turso SDK packages are exact 0.7.2 source
copies with Rust-only library packaging: they no longer build unused standalone
C dynamic/static libraries or their Windows DLL version-resource script. Their
Rust source and dependency features are unchanged. See `vendor/README.md` for
provenance and the update procedure. They are excluded from workspace membership.

TOML, TOML-edit, quick-xml and serde_json use optimization level 1. SHA-2,
memchr and rand_core use level 3 for hashing, byte scanning and RNG helpers
identified in full-seed CPU profiles. These are targeted overrides, not blanket
optimization of third-party dependencies. Level 1 preserves
[cross-crate generic code sharing](https://doc.rust-lang.org/cargo/reference/profiles.html#overrides-and-generics);
debug assertions and overflow checks remain enabled. Measure clean builds,
edit/rebuilds and test execution separately when changing these overrides.

Overlapping actor-runtime PostgreSQL fixtures lease one server but retain a fresh
database, complete schema and runtime-local connection pool per test. Tables,
sequences and advisory locks remain database-isolated. A weak reference does not
keep the server alive after the last test lease drops. The explicit local database
URL opt-in retains its existing behavior; older shared fixtures described above
are unchanged.

Full workspace runs register the actor-runtime's sixteen PostgreSQL proofs in the
shared server integration harness, so Docker startup can overlap other scenarios.
The ten unit proofs use the `actor_runtime_postgres::` module filter and the six
integration proofs use `pg_actor_integration::`. Separating the groups leaves
worker slots available for CPU-only tests while the first PostgreSQL fixtures start.
The non-default `test-shared-pg-proofs` feature exports the original proof bodies
without changing them and suppresses their duplicate unit- and integration-test
registrations. The concurrent-creation proof retains its three-worker multi-thread
runtime, and every fixture and the explicit external database opt-in are unchanged.
Only the integration package's dev-dependency enables it. Standalone
`cargo test -p temper-actor-runtime` still runs those proofs in their original unit
and integration harnesses with their original test names.
Do not explicitly enable this layout feature without also running the integration
package. The existing inventory regression checks complete, unique registration
and matching Tokio runtime configurations.

The same server harness also runs the agents crate's three original PostgreSQL
chain proofs under `pg_agent_chain::`, preserving every poll, sleep, scheduler
restart and assertion. The separate non-default
`temper-agents/test-shared-pg-proofs` feature is likewise enabled only by the
integration package's dev-dependency. It exports the unchanged owner-relative
source and suppresses duplicate registrations; standalone
`cargo test -p temper-agents --test agent_chain` retains all three original names.
The existing inventory regression covers all nineteen PostgreSQL proof wrappers
and their exact Tokio runtime configurations. Normal production features do not
include either test-harness bridge.

The pinned nightly compiler uses sixteen frontend workers (`-Zthreads=16`).
Cargo's jobserver coordinates them with other compiler jobs; this is not sixteen
additional unbounded threads per crate. The macOS ARM target retains its existing
eight-worker configuration and Z3 library search path.

Local builds and CI pin the same September 26, 2026 nightly. It includes newer
trait-query caching than the February pin. The compiler's default borrow checker
remains enabled; opting out of Polonius Alpha did not improve the measured run.
The server and platform use a bounded recursion limit of 256 for their existing
async `Send` proofs. The newer solver counts cached proof depth more accurately;
these proofs succeed at that depth without unsafe implementations or disabling
the compatibility warning. See the [Rust compiler tracking issue](https://github.com/rust-lang/rust/issues/159228).

Native builds use the exact published `cc` 1.4.5 helper with a Linux-host polling
adjustment. Its unchanged jobserver limits still bound compiler concurrency, but
idle checks back off from 1 to 10 milliseconds rather than 100 to 1,000. This
avoids adding a long polling interval to each short C/assembly compilation.
Other hosts retain upstream behavior; compiler flags, native sources and test
workloads are unchanged. See `vendor/cc/README.temper.md` for provenance and checks.

GNU/Linux x86_64 and aarch64 builds select the upstream prebuilt Z3 4.14.1 solver
instead of the host's potentially much older package. An empty target downloads
and extracts its release archive; the upstream downloader does not separately
verify a checksum. The pinned archives require glibc 2.35 on x86_64 and 2.34 on
aarch64, compatible with the existing Ubuntu 22.04 release jobs and bookworm
runtime. Newer x86_64 archives require glibc 2.39. Other targets keep their
existing system-library integration. Queries still use fresh independent
solvers, unchanged premises and conservative handling of unknown results.

## Policy compilation reuse

Cedar engines reuse immutable compiled policy snapshots for identical raw source
text. The cache keeps at most 64 entries and 1 MiB of source text; sources larger
than 64 KiB compile normally without admission. Only successful compilation is
cached, not authorization decisions, errors, credentials or entity data.

Each engine still owns its tenant activation and fallback selection. Reloading or
removing a tenant replaces only that engine's snapshot. Recovery still reads the
current durable policy rows, and changed text selects its matching snapshot. Named
policies retain tenant-qualified IDs and are compiled independently. The constant
built-in platform policy is immutable and compiled once per process.

## Prepared IOA reuse

IOA parsers reuse immutable prepared automata for byte-identical source text,
bounded to 128 entries and 2 MiB of source text; sources over 128 KiB parse
normally without admission. Each caller still reads the current liveness mode,
checks coverage, emits warnings and invokes violation reporters. Errors and
liveness rejections do not populate or evict cache entries. Returned automata
are independently owned: mutations and eviction cannot change another caller's
result. Source reads and durable recovery remain uncached.

CSDL parsing also reuses independently owned documents for exact XML source
bytes, with the same 128-entry, 2 MiB source-text and 128 KiB admission budgets.
Errors are never cached. Documents containing Record annotations are never
retained: their unordered maps still need fresh construction so caching cannot
change the existing XML emitter's member-order behavior. Tenant registries,
metadata merging and durable source reads remain independent and uncached.

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

Private dispatch boundaries similarly expose compiler-checked `Send` proofs for
both ordinary and typed/conditional mutations. They return the original async
futures directly: no extra allocation, eager work, changed tracing, authorization
precondition, reaction order or cancellation behavior.

The checked and public File content-upload entry points, legacy tenant dispatch,
and tenant ensure/registration methods expose the same allocation-free `Send`
boundaries. Workspace guards, native/WASM fallback, authorization preconditions,
blob-write order, SQL, registration and poll-time tracing stay in the original
async implementations. Regression checks retain the original empty tracing-field
declarations and nested native-upload span as well as laziness and error mapping.

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

MCP unit tests prepare one immutable native TLS connector to avoid repeatedly
loading the trust store. Each context or operation still builds a fresh reqwest
client and connection pool, preserving its original timeout and redirect policy.
The helper is `cfg(test)` only; production MCP client constructors are unchanged.
An actual socket regression checks pool separation, per-request headers and the
different redirect policies. Trust-store changes during one test process are not
reloaded by the prepared connector.

WASM tests similarly prepare default TLS once, but keep fresh public and
internal clients and each engine's separate pools. Valid custom CA certificates
always use the original per-configuration TLS builder; invalid certificates
retain their warnings and cache-key separation. Real loopback HTTPS checks cover
custom trust and hostname rejection, alongside pool and timeout checks. Production
builders are unchanged. This test-only default preparation also does not reload
system root-store contents during a process; tests of such rotation must bypass it.
The server, platform and integration harness enable `test-prepared-default-tls`
only on dev-dependencies, like the separate compiled-code cache. Normal production
builds enable neither feature.

The two fake-operation Turso retry tests use paused Tokio time rather than real
backoff sleeps. They exercise the unchanged retry function and assert every
250/500/1,000/2,000 ms interval, all attempts and no extra final delay. Production
retry timing and real SQL connection-lifetime stress remain unchanged.

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
