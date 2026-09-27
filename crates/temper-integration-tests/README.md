# Cross-crate integration tests

`cargo test --workspace --no-fail-fast` includes every server and platform
integration suite in this test-only package. The original source files and
fixtures stay under their owning crates; the `server` and `platform` harnesses
import them as modules.

This avoids recompiling and linking one executable per source file, and lets
ordinary Cargo tests reuse the immutable process-local WASM compilation cache.
Runtime state, actor registries and guest stores are still created per test.
Server unit tests no longer depend on the server library through a self
dev-dependency or through the platform crate, so they can compile alongside it.

Targeted commands now use the original suite name as a module filter:

```sh
cargo test -p temper-integration-tests --test server dst_platform_random::
cargo test -p temper-integration-tests --test platform platform_e2e_dst::
```

`cargo test -p temper-server` runs that package's unit and binary tests, not its
cross-crate integration suites. Its `actor_throughput` benchmark remains in the
server package with the same `cargo bench -p temper-server --bench actor_throughput`
command. The `observe` feature is enabled by default for this package and gates
the same observe-only modules as before.

The server harness compiles shared helper code once. Small forwarding tests
retain both original workload-generator checks in every suite that previously
included them: 17 suites, 34 helper-test executions with unchanged assertions.
The inventory regression also rejects unregistered root-level server or platform
test files. When adding a suite, add its source-path module to the relevant harness.

The server harness also runs the actor-runtime's sixteen original PostgreSQL proofs
through `actor_runtime_postgres::` (ten unit proofs) and `pg_actor_integration::`
(six integration proofs) wrappers. Their separate module names leave worker slots
available for CPU-only tests while Docker starts. The dev-only `test-shared-pg-proofs`
feature leaves their bodies in the actor crate, where private details stay private,
and disables only duplicate unit- and integration-test registrations. Standalone
actor-runtime defaults retain the original registrations and test names. The
inventory guard checks exact coverage, registration exclusivity, and each Tokio
runtime configuration, including the concurrent-creation proof's three-worker
multi-thread runtime. Fixtures, connection pools, and the explicit external
database opt-in remain unchanged.

The agents crate's three original `agent_chain` PostgreSQL proofs also run in the
same server executable through `pg_agent_chain::`. Its separate non-default
`temper-agents/test-shared-pg-proofs` feature is enabled only by this package's
dev-dependency, compiling the unchanged bodies in their owner. Standalone
`cargo test -p temper-agents --test agent_chain` retains all three original test
names. Polls, sleeps, scheduler restarts, fixtures and assertions are unchanged;
the existing inventory regression checks their registrations and Tokio attributes.
