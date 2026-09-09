# Temper

The Temper kernel: specs, verification, runtime, server, stores, observe, evolution. **Kernel code only** - app and agent logic belongs in temperpaw; if you find it mixed in here, flag it, do not silently relocate it. Global rules (worktrees, PRs, reviews, Definition of Done) come from the stack layer; this file is what is temper-specific.

Worktrees live under `~/Development/temper-worktrees/` (the primary at `~/Development/temper` is bare on purpose).

## Repo map

- `crates/temper-spec` - how defined: IOA + CSDL parsers
- `crates/temper-jit` - what is defined: `TransitionTable`, `Effect`
- `crates/temper-verify` - how verified: L0-L3 cascade (own `ModelEffect`)
- `crates/temper-runtime` - runtime (replaceable): mailbox, sim, `EventStore` trait; WASM host in `temper-wasm` + `temper-wasm-sdk`
- `crates/temper-store-turso` - default store; `-sim`, `-postgres`, `-redis` (journal only) are plugs
- `crates/temper-server` - the mix, read last: HTTP + EntityActor + `apply_effects` + registry
- Control plane: `temper-odata`, `temper-authz`, `temper-observe`, `temper-evolution`, `temper-ots`, `temper-platform`, `temper-cli`, `temper-mcp`, `temper-sdk`, `temper-sandbox`
- Default serve is own-Rust actors; `--actor-runtime postgres` is an adapter, not a second kernel. `apply_effect` must become one function; today it is three.
- Verification: `.agents/skills/verify-temper/` - the verification skill and feature map
- Architecture Decision Records: `docs/adrs/` (template at `docs/adrs/TEMPLATE.md`). A material architecture change gets its ADR before code - required for new features, new integrations, multi-crate changes, new patterns; not for bug fixes, single-file refactors, docs, or test additions.

## Specs

- IOA TOML (`.ioa.toml`) is the spec format; `TransitionTable::from_ioa_source()` in production. TLA+ is legacy: `from_tla_source()` is `#[cfg(test)]` only.
- Specs are generated from conversation, never hand-written; code derived from specs is regenerable.
- Framework code must not hardcode entity-specific state names. Domain invariants come from the spec's `[[invariant]]` sections. The verification cascade gates every spec change.
- Data contract is CSDL (`*.csdl.xml`); a running server publishes it at `GET /tdata/$metadata`.

## Deterministic simulation (DST)

In simulation-visible crates (`temper-runtime`, `temper-jit`, `temper-server`):

- `sim_now()` / `sim_uuid()`, never wall clock or random UUIDs
- `BTreeMap`/`BTreeSet`, never `HashMap`/`HashSet` (deterministic iteration)
- No `std::thread::spawn`, `rayon`, multi-threaded `tokio::spawn` - single-threaded actor model
- No `std::fs`, `std::net`, `std::env::var` - I/O behind traits
- No `static mut`, `lazy_static!`, `thread_local!` - state through actor context
- No `chrono::Utc::now()`, `std::thread::sleep()`, `OsRng`, `getrandom` - simulated time, seeded PRNG
- `SimActorHandler::spec_invariants()` auto-checks `[[invariant]]` sections
- `// determinism-ok` suppresses guard false positives
- Full ruleset: `.agents/agents/dst-reviewer.md`; method: `.agents/skills/deterministic-simulation/`

DST-driven development replaces TDD for stateful kernel code:

1. Harness first: extend the simulator with the scenario - workload, faults, invariants (the things that must never happen). The invariant must FAIL before you implement; a harness that cannot catch the missing behavior proves nothing.
2. Implement. Production code runs inside the simulation - not mocks, not a parallel reimplementation.
3. Run many seeds until invariants hold. One green seed is one execution, not correctness.
4. A failing seed found later is committed as a regression case and stays in the suite forever. The seed is the bug report: seed, invariant violated, minimal trace.
5. Fix by root cause. Never by weakening the invariant or narrowing the workload.

Suites: `platform_e2e_dst`, `system_entity_dst` (crates/temper-platform/tests/). Cooperative fault points (BUGGIFY-style rare branches under simulation) belong in production code.

## Multi-tenancy and identity

- `SpecRegistry` maps (TenantId, EntityType) to specs + TransitionTable; single-tenant is `TenantId::default()` = "default". Pass the tenant explicitly; never assume a hardcoded one.
- Agent identity registry: the platform verifies the `agent_type` claim and sets `agentTypeVerified` on the Cedar principal. Policies treat unverified types as untrusted.

## Rust conventions

- Edition 2024, rust-version 1.85. `gen` is a reserved keyword.
- Files over 500 lines split into directory modules. All pub items documented.
- TigerStyle: bounded mailboxes, pre/post assertions at function entry and return, budgets not limits, fail fast on invariant violation, no silent failures.
- `temper-jit` must not depend on `temper-verify` in `[dependencies]`. Production binaries must not pull in `stateright` or `proptest`.

## Commands

```bash
cargo test --workspace                                  # full suite
cargo test -p temper-platform --test platform_e2e_dst   # E2E shared-registry proof
cargo run -p temper-cli -- serve --port 3000            # HTTP server, OData API, Observe UI
scripts/setup-hooks.sh                                  # install git hooks (pre-commit integrity)
```

## Enforcement hooks

`.claude/settings.json` wires blocking hooks: L0-L3 spec verification on every `.ioa.toml` edit, a 25-pattern determinism guard on `.rs` edits in sim-visible crates, and a pre-commit gate requiring DST-review and code-review markers (`.agents/agents/dst-reviewer.md`, `code-reviewer.md` write them on PASS). Tests are not run by local hooks; CI runs the full suite, the DST matrix and the readability ratchet on every pull request and on pushes to main/staging (ci.yml `on:`). A branch push with no PR starts nothing.

## Deploying spec changes

1. Spec passes the verification cascade (L0-L3)
2. TransitionTable builds from the verified spec
3. Entity actors hot-deploy without dropping existing state
4. OData endpoints respond for all entity types
5. Telemetry emits WideEvents for all transitions
