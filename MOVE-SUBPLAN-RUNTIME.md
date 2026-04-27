# Subplan: Move temper-runtime + temper-macros to dd-source

> Part of [MOVE-TO-DD-SOURCE.md](./MOVE-TO-DD-SOURCE.md) — Layer 0 + Layer 1 (partial)

## What We're Moving

### temper-macros (30 lines, 0 internal deps)

Proc-macro crate. Two derive macros:
- `#[derive(Message)]` → implements `temper_runtime::actor::Message`
- `#[derive(DomainEvent)]` → implements `temper_runtime::persistence::DomainEvent`

External deps: `syn 2`, `quote 1`, `proc-macro2 1` (standard proc-macro stack).

**⚠️ Important**: The generated code references `temper_runtime::` paths directly.
Any crate using these macros MUST also depend on `temper-runtime`. This is the normal
pattern for derive macros (like serde_derive → serde). No action needed, just noting it.

Currently **unused** — no other crate in the workspace depends on temper-macros.
The macros work but nobody is using the derives yet (they implement Message and
DomainEvent manually). We move it anyway since it's trivial and part of the runtime
foundation.

### temper-runtime (3532 lines, 0 internal deps)

The core actor framework. Erlang/Akka-inspired, tokio-based.

```
temper-runtime/src/
├── lib.rs              ← re-exports ActorSystem, TenantId, QualifiedEntityId
├── actor/
│   ├── mod.rs          ← re-exports Actor, Message, ActorRef, ActorContext, ActorError
│   ├── traits.rs       ← Actor trait, Message marker trait
│   ├── actor_ref.rs    ← ActorRef<Msg>, ActorId, SystemSignal
│   ├── context.rs      ← ActorContext (self ref, children, persistence)
│   ├── cell.rs         ← ActorCell (message loop, lifecycle, supervision)
│   └── errors.rs       ← ActorError enum
├── buggify.rs          ← Deterministic fault injection (DST)
├── mailbox/mod.rs      ← Bounded async mailbox (tokio mpsc)
├── persistence/mod.rs  ← DomainEvent, EventStore, PersistentActor, snapshots
├── scheduler/
│   ├── mod.rs          ← sim_now(), sim_uuid(), deterministic context
│   ├── clock.rs        ← SimClock (deterministic time for DST)
│   ├── context.rs      ← Thread-local deterministic context
│   ├── id_gen.rs       ← Deterministic UUID generation
│   ├── sim_actor_system.rs ← SimActorSystem for model checking
│   └── sim_handler.rs  ← SimActorHandler, SpecAssert, SpecInvariant
├── supervision/mod.rs  ← SupervisionStrategy (restart, stop, escalate)
├── system/mod.rs       ← ActorSystem (spawn, find, shutdown)
└── tenant/mod.rs       ← TenantId, QualifiedEntityId, parse_persistence_id_parts
```

External deps (all standard, no surprises):
```toml
tokio = { features = ["full"] }
serde = { features = ["derive"] }
serde_json = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
uuid = { features = ["v4", "v7", "serde"] }
chrono = { features = ["serde"] }
```

Dev deps: `tokio-test = "0.4"` only.

### Who depends on temper-runtime? (16 crates — everything except temper-macros and temper-wasm-sdk)

```
temper-agentos, temper-authz, temper-cli, temper-evolution,
temper-jit, temper-mcp, temper-observe, temper-odata,
temper-optimize, temper-platform, temper-server,
temper-store-postgres, temper-store-redis, temper-store-sim,
temper-store-turso, temper-verify
```

Most commonly imported types:
- `temper_runtime::tenant::TenantId` — used everywhere
- `temper_runtime::scheduler::{sim_now, sim_uuid}` — DST helpers
- `temper_runtime::persistence::*` — EventStore, PersistentActor, DomainEvent
- `temper_runtime::actor::*` — Actor, ActorContext, ActorError, Message
- `temper_runtime::ActorSystem` — top-level system

**No circular deps.** Runtime is a pure leaf. Nothing in runtime imports from other temper crates.

## Pre-Flight Checks

- [x] `cargo test -p temper-runtime -p temper-macros` passes (all green)
- [x] Zero internal deps (both are leaf crates)
- [x] External deps are all standard crates (tokio, serde, uuid, chrono, syn, quote)
- [x] Gumshoe precedent exists in dd-source (Rust workspace + Bazel)
- [x] ✅ **No nightly needed!** temper-runtime + temper-macros compile and pass 65 tests on **stable 1.90.0** (dd-source's pinned version). See toolchain analysis below.
- [x] Confirm `rules/rust/defs.bzl` exports `dd_rust_proc_macro` for proc-macro crates ✅

## Steps

### Step 1: Create workspace scaffold

```bash
# In dd-source
mkdir -p domains/odp/temper/crates
```

Create `domains/odp/temper/Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = [
    "crates/temper-macros",
    "crates/temper-runtime",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"
rust-version = "1.90"

[workspace.dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Error handling
thiserror = "2"
anyhow = "1"

# Logging
tracing = "0.1"

# UUID + time
uuid = { version = "1", features = ["v4", "v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }

# Proc macro support
syn = { version = "2", features = ["full"] }
quote = "1"
proc-macro2 = "1"

# Testing
tokio-test = "0.4"

# Internal crates
temper-macros = { path = "crates/temper-macros" }
temper-runtime = { path = "crates/temper-runtime" }
```

Create `domains/odp/temper/rust-toolchain.toml` (match dd-source root):
```toml
[toolchain]
channel = "1.90.0"
profile = "default"
targets = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "wasm32-wasip1",
    "x86_64-unknown-linux-gnu",
]
```

### Step 2: Copy crate sources

```bash
# From temper repo to dd-source
cp -r ~/go/src/github.com/DataDog/temper/crates/temper-macros \
      ~/dd/dd-source/domains/odp/temper/crates/temper-macros

cp -r ~/go/src/github.com/DataDog/temper/crates/temper-runtime \
      ~/dd/dd-source/domains/odp/temper/crates/temper-runtime
```

### Step 3: Add BUILD.bazel files

`domains/odp/temper/BUILD.bazel`:
```python
load("//rules/rust:defs.bzl", "dd_cargo_dot_toml")

dd_cargo_dot_toml(src = "Cargo.toml")
```

`domains/odp/temper/crates/temper-macros/BUILD.bazel`:
```python
load("//rules/rust:defs.bzl", "dd_cargo_dot_toml", "dd_rust_proc_macro")
load("//third_party/crates:defs.bzl", "all_crate_deps")

dd_cargo_dot_toml(src = "Cargo.toml")

dd_rust_proc_macro(
    name = "temper-macros",
    srcs = glob(["src/**/*.rs"]),
    crate_name = "temper_macros",
    visibility = ["//domains/odp:__subpackages__"],
    deps = all_crate_deps(normal = True),
)
```

> **✅ Confirmed**: `dd_rust_proc_macro` exists in `rules/rust/defs.bzl`
> (wrapper around upstream `rules_rust`'s `rust_proc_macro`).
> Used by multiple domains (monitor-intake, obs-pipelines, service-config, etc.).

`domains/odp/temper/crates/temper-runtime/BUILD.bazel`:
```python
load("//rules/rust:defs.bzl", "dd_cargo_dot_toml", "dd_rust_library", "dd_rust_test")
load("//third_party/crates:defs.bzl", "all_crate_deps")

dd_cargo_dot_toml(src = "Cargo.toml")

dd_rust_library(
    name = "temper-runtime",
    srcs = glob(["src/**/*.rs"]),
    crate_name = "temper_runtime",
    visibility = ["//domains/odp:__subpackages__"],
    deps = all_crate_deps(normal = True),
)

dd_rust_test(
    name = "test",
    crate = ":temper-runtime",
    deps = all_crate_deps(normal_dev = True),
)
```

### Step 4: Verify

> **⚠️ dd-source uses `bzl` (Bazel) as the primary build system, NOT `cargo`.**
>
> From Confluence + gumshoe README:
> - `bzl build` / `bzl test` are the canonical commands
> - `cargo` is used **only** for examples and local experiments (e.g. `cargo run --example`)
> - CI gates on Bazel, not cargo
> - `rust-analyzer` can be wired through Bazel for IDE support (see Confluence:
>   "VS Code + Bazel + Rust + rust-analyzer")

```bash
cd ~/dd/dd-source

# Primary: Bazel (what CI runs)
bzl build //domains/odp/temper/...
bzl test //domains/odp/temper/...

# Optional: cargo for quick local iteration (NOT the CI gate)
# cd domains/odp/temper && cargo check --workspace && cargo test --workspace
```

### Step 5: PR

Single PR: "feat(odp/temper): bootstrap workspace with temper-runtime and temper-macros"

Contents:
```
domains/odp/temper/
├── Cargo.toml
├── rust-toolchain.toml
├── BUILD.bazel
└── crates/
    ├── temper-macros/
    │   ├── Cargo.toml
    │   ├── BUILD.bazel
    │   └── src/lib.rs
    └── temper-runtime/
        ├── Cargo.toml
        ├── BUILD.bazel
        └── src/
            ├── lib.rs
            ├── actor/     (6 files)
            ├── buggify.rs
            ├── mailbox/   (1 file)
            ├── persistence/ (1 file)
            ├── scheduler/ (6 files)
            ├── supervision/ (1 file)
            ├── system/    (1 file)
            └── tenant/    (1 file)
```

## Toolchain Analysis

### dd-source pins

```
rust-toolchain.toml:  channel = "1.90.0" (stable)
WORKSPACE:            edition = "2024", versions = ["1.90.0"]
                      rules_rust v0.67.0
```

### Temper pins

```
rust-toolchain.toml:  channel = "nightly-2026-02-08" (= rustc 1.95.0-nightly)
Cargo.toml:           rust-version = "1.92"
```

### Can we use dd-source's stable 1.90.0?

**YES for temper-runtime + temper-macros.** Verified:

- `cargo check` ✅ on 1.90.0 (after lowering `rust-version`)
- `cargo test` ✅ — all 65 tests pass on 1.90.0
- Zero `#![feature(...)]` gates anywhere in the workspace
- No nightly-only syntax (no bare `gen`, no nightly APIs)

The `rust-version = "1.92"` in Cargo.toml is a **soft MSRV** — it was set aspirationally
but nothing actually requires 1.92. We should lower it to `"1.90"` for dd-source compatibility.

### What actually requires > 1.90?

| Crate | Requires | Why |
|---|---|---|
| `ruff_python_*` | 1.91 | Pulled in by `monty` → `temper-mcp` (Python type checking). Not moving in layer 1. |
| `temper-wasm-sdk` | 1.92 (MSRV set) | Needs investigation when we move it. |
| `ecommerce-reference` | 1.92 (MSRV set) | Not moving to dd-source. |
| `oncall-reference` | 1.92 (MSRV set) | Not moving to dd-source. |

**For the temper-runtime + temper-macros PR**: set `rust-version = "1.90"` and match dd-source exactly. No nightly, no toolchain friction.

**For later layers** (temper-mcp, temper-wasm-sdk): may need to either:
1. Wait for dd-source to bump to 1.91+, or
2. Pin `monty`/`ruff` to an older version, or
3. Make the MCP Python type checker optional via feature flag

## Dependency Analysis: What Future Layers Will Need

Here's what temper-runtime exports that downstream crates consume. This is relevant
because when we move layer 2+, those crates will `path = "../temper-runtime"` to the
dd-source copy:

| Export | Used by |
|---|---|
| `ActorSystem` | server, platform, mcp, cli |
| `Actor`, `ActorContext`, `ActorError`, `Message` | server (EntityActor) |
| `TenantId`, `QualifiedEntityId` | server, platform, cli, stores |
| `parse_persistence_id_parts` | store-postgres, store-sim, store-turso |
| `EventStore`, `PersistentActor`, `DomainEvent` | server, all stores |
| `PersistenceEnvelope`, `PersistenceError` | server, all stores |
| `sim_now()`, `sim_uuid()` | server, agentos, observe (DST only) |
| `SimActorSystem`, `SimActorHandler` | server (DST simulation) |
| `SupervisionStrategy` | server (EntityActor) |
| `buggify` module | (available but usage TBD) |

No surprises — temper-runtime is the foundation. Every subsequent layer depends on it.
Moving it first is the right call.
