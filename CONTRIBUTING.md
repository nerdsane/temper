# Contributing to Temper

Thank you for considering contributing to Temper! This document provides guidelines and information for contributors.

## Getting Started

```bash
# Clone the repository
git clone https://github.com/nerdsane/temper.git
cd temper

# Build the project
cargo build --workspace

# Run the test suite
cargo test --workspace
```

## Development Requirements

- **Rust** 1.92+ (Edition 2024)
- **PostgreSQL** for integration tests (or use Turso for local dev)
- **Z3** for SMT verification (L0 cascade level)

## Project Structure

Temper is a Cargo workspace with 19 crates. See the [README](README.md) for the full architecture overview. Key crates for contributors:

- `temper-spec` — Start here to understand the specification format
- `temper-verify` — The four-level verification cascade
- `temper-server` — HTTP server and entity dispatch
- `temper-runtime` — Actor system and event sourcing

## Coding Standards

### Deterministic Simulation

Code in simulation-visible crates (`temper-runtime`, `temper-jit`, `temper-server`) must be deterministic:

- Use `sim_now()` instead of wall clock time
- Use `sim_uuid()` instead of random UUIDs
- Use `BTreeMap`/`BTreeSet` instead of `HashMap`/`HashSet`
- No `std::thread::spawn`, `rayon`, or multi-threaded `tokio::spawn`
- No `std::fs`, `std::net`, or `std::env::var` — abstract I/O behind traits

### TigerStyle Engineering

- Bounded mailboxes — every actor mailbox has a capacity limit
- Pre-assertions at function entry, post-assertions before return
- Budgets not limits — express constraints as budgets that get consumed
- Fail fast on invariant violations
- No silent failures — every error path is logged or propagated

### Code Organization

- Files exceeding 500 lines must be split into directory modules
- All `pub` items must have doc comments
- `gen` is a reserved keyword in Edition 2024 — never use as a variable name

### Dependency Discipline

- `temper-jit` must not depend on `temper-verify` in `[dependencies]`
- Production binaries must not pull in `stateright` or `proptest`

## Pull Request Process

1. Fork the repository and create a feature branch
2. Write tests for new functionality
3. Ensure all tests pass: `cargo test --workspace`
4. Run clippy: `cargo clippy --workspace`
5. Run formatting: `cargo fmt --check`
6. Submit a pull request with a clear description

## Architecture Decision Records

Significant changes require an ADR. Create `docs/adrs/NNNN-short-title.md` following the template at `docs/adrs/TEMPLATE.md`. Required for:

- New features or architectural changes
- New integrations or multi-crate changes
- New patterns or conventions

Not required for bug fixes, single-file refactors, documentation changes, or test additions.

## License

By contributing, you agree that your contributions will be dual-licensed under the MIT and Apache 2.0 licenses.
