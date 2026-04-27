# Subplan: Port `temper-spec` to dd-source

**Status**: Planned
**Depends on**: temper-runtime PR (#379148) merged or at least branch exists
**Estimated effort**: ~15 min (process well-understood from runtime port)

## What is temper-spec?

CSDL + I/O Automaton + TLA+ specification parser. This is the
"specs are source of truth" crate — everything downstream depends on it.

| Metric | Value |
|--------|-------|
| Lines of Rust | 5,285 |
| Source files | 20 |
| Unit tests | 62 |
| Test fixtures | None (all inline) |
| Async? | No — pure sync parsing |

## Module structure

```
temper-spec/src/
├── lib.rs                      # Re-exports primary API
├── automaton/                  # I/O Automaton TOML parser (primary format)
│   ├── mod.rs
│   ├── types.rs         (294)  # Automaton, Action, Variable, etc.
│   ├── parser.rs        (803)  # TOML → Automaton
│   ├── toml_parser.rs   (836)  # Low-level TOML field extraction
│   ├── assert_parser.rs (258)  # Assertion expression parser
│   ├── initial.rs        (95)  # Initial-state value parsers
│   ├── lint.rs          (306)  # Lint checks on parsed automata
│   └── metadata.rs      (232)  # Metadata block parsing
├── cross_invariant/            # Cross-entity invariant specs
│   ├── mod.rs
│   ├── types.rs          (83)
│   ├── parser.rs        (237)
│   └── lint.rs          (104)
├── csdl/                       # OData CSDL XML parser
│   ├── mod.rs
│   ├── types.rs         (271)
│   └── parser.rs        (676)  # quick-xml based
├── model/                      # Unified SpecModel builder
│   └── mod.rs           (241)
└── tlaplus/                    # Legacy TLA+ extractor
    ├── mod.rs
    ├── types.rs          (57)  # StateMachine, Transition, Invariant
    └── extractor.rs     (664)  # Regex-based TLA+ → StateMachine
```

## Dependencies

```toml
[dependencies]
quick-xml  = { version = "0.37", features = ["serialize"] }  # NOT in dd-source yet
toml       = "0.8"                                            # NOT in dd-source yet
serde      = { version = "1", features = ["derive"] }         # already in workspace
serde_json = "1"                                              # already in workspace
thiserror  = "2"                                              # already in workspace
```

**No inter-crate deps** — temper-spec does NOT depend on temper-runtime or temper-macros.

## Downstream consumers (future ports, not this PR)

```
temper-verify ──┐
temper-codegen ─┤
temper-jit ─────┤
temper-cli ─────┼── all depend on temper-spec
temper-server ──┤
temper-platform ┤
temper-evolution┤
temper-mcp ─────┘
```

## Steps

### 1. Add new workspace deps to root `Cargo.toml`

```toml
# In [workspace.dependencies], add:
quick-xml = { version = "0.37", features = ["serialize"] }
toml = "0.8"
```

### 2. Register temper-spec in root `Cargo.toml`

```toml
# In [workspace] members, add:
"domains/odp/temper/crates/temper-spec",

# In [workspace.dependencies], add:
temper-spec = { path = "domains/odp/temper/crates/temper-spec" }
```

### 3. Copy source via rsync

```bash
rsync -av \
  ~/go/src/github.com/DataDog/temper/crates/temper-spec/src/ \
  ~/dd/dd-source/domains/odp/temper/crates/temper-spec/src/
```

### 4. Write `crates/temper-spec/Cargo.toml`

```toml
[package]
name = "temper-spec"
version.workspace = true
edition.workspace = true
license = "MIT OR Apache-2.0"
description = "CSDL + TLA+ specification parser for the Temper framework"

[dependencies]
quick-xml = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
toml = { workspace = true }
```

### 5. Write `crates/temper-spec/BUILD.bazel`

```python
load("//rules/rust:defs.bzl", "dd_cargo_dot_toml", "dd_rust_library", "dd_rust_test")
load("//third_party/crates:defs.bzl", "all_crate_deps")

dd_cargo_dot_toml(src = "Cargo.toml")

dd_rust_library(
    name = "temper-spec",
    srcs = glob(["src/**/*.rs"]),
    crate_name = "temper_spec",
    visibility = ["//domains/odp:__subpackages__"],
    deps = all_crate_deps(normal = True),
)

dd_rust_test(
    name = "test",
    crate = ":temper-spec",
    deps = all_crate_deps(normal_dev = True),
)
```

### 6. Format source

```bash
cd ~/dd/dd-source && cargo fmt -- domains/odp/temper/crates/temper-spec/src/**/*.rs
```

### 7. Fix doc-links (if any break under `-D rustdoc::broken-intra-doc-links`)

Only 4 doc-links found in the crate — all reference types within temper-spec
itself, so they should resolve. Verify after build.

### 8. Regenerate crate vendor

```bash
cd ~/dd/dd-source && bzl run //third_party:crates_vendor
```

This adds `quick-xml` and `toml` (+ their transitive deps) to `third_party/crates/defs.bzl`.
Expect ~3 min runtime.

### 9. Build and test

```bash
bzl build //domains/odp/temper/...
bzl test  //domains/odp/temper/...
```

Expected: 187 unit tests (125 runtime + 62 spec), all green.

### 10. PR

Branch: `gbaldoni/no-ticket/temper-spec-to-dd-source`

## What's different from the runtime port

| Aspect | Runtime | Spec |
|--------|---------|------|
| New third-party deps | 0 (all already in workspace) | 2 (`quick-xml`, `toml`) |
| Inter-crate deps | None | None |
| Async / tokio | Yes | No |
| Test fixtures | None | None |
| Proc macros | temper-macros companion | N/A |
| `license.workspace` trap | Hit it, fixed | Already know — use inline |
| `rust-version.workspace` trap | Hit it, fixed | Already know — skip it |

## Risks

- **Low**: `quick-xml 0.37` or `toml 0.8` might conflict with versions used
  elsewhere in dd-source. Mitigation: check `Cargo.lock` after vendor for
  duplicate semver entries.
- **Low**: crates_vendor may pull in new transitive deps that take time to
  compile. Not a correctness risk, just CI time.
