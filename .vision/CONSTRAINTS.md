# Temper Constraints

## Non-Negotiable Rules

### No Entity-Specific Hardcoding
Framework code must never contain entity-specific state names, action names, or business logic. All entity knowledge lives in specs. Framework code operates on generic spec-derived structures (TransitionTable, state IDs, action IDs).

### Deterministic Simulation
- Use `sim_now()` instead of wall clock time
- Use `sim_uuid()` instead of random UUIDs
- Use `BTreeMap` not `HashMap` in simulation-visible code (deterministic iteration order)
- All actor scheduling must go through `SimScheduler` for reproducible test runs

### Dependency Isolation
- `temper-jit` must NOT depend on `temper-verify` in `[dependencies]` (only `[dev-dependencies]` is allowed)
- Production binaries must not pull in `stateright` or `proptest`
- Verification crates are design-time only

### Rust Edition and Toolchain
- Edition 2024, rust-version 1.85
- `gen` is a reserved keyword in Edition 2024 — never use as a variable name

### Code Organization
- Files exceeding 500 lines must be split into directory modules (e.g., `foo.rs` becomes `foo/mod.rs` + `foo/bar.rs`)
- All `pub` items must have doc comments

### TigerStyle Engineering
- **Bounded mailboxes**: Every actor mailbox has a capacity limit, never unbounded
- **Pre-assertions**: Validate inputs at function entry with `assert!` or `debug_assert!`
- **Post-assertions**: Validate outputs before return
- **Budgets not limits**: Express constraints as budgets that get consumed, not arbitrary limits
- **Fail fast**: If an invariant is violated, panic immediately rather than propagating corrupt state
- **No silent failures**: Every error path must be logged or propagated
