# Subplan: Move temper-authz to dd-source

> Part of [MOVE-TO-DD-SOURCE.md](./MOVE-TO-DD-SOURCE.md) — Layer 3 (authorization)

## What We're Moving

### temper-authz (946 lines, 1 internal dep)

Cedar-based authorization engine. Every actor message dispatch goes through:

```
HTTP Request → X-Temper-* headers → SecurityContext → Cedar Evaluate → Allow/Deny
```

5 source files:

| File | Lines | Role |
|---|---|---|
| `context.rs` | 262 | Extracts `SecurityContext` from HTTP headers. 4 principal kinds: Customer, Agent, Admin, System. Promotes anonymous→Agent for WASM modules. |
| `engine.rs` | 514 | Core `AuthzEngine` — wraps `cedar-policy`. Compiles Cedar policy text, maps Temper concepts to Cedar (principal kinds→Cedar types, OData actions→`Action::"read"`, etc.). Supports **hot-reload** via `RwLock`. System principals bypass all checks. |
| `integration_gate.rs` | 127 | `IntegrationAuthzGate` trait for authorizing outbound calls from WASM integrations (HTTP calls, secret access). Includes `extract_domain` with SSRF protection. |
| `error.rs` | 25 | `AuthzError` enum (thiserror) |
| `lib.rs` | 18 | Re-exports |

### How it works

**Cedar mapping:**
```
PrincipalKind::Admin  → Cedar type Admin::"admin-1"
PrincipalKind::Agent  → Cedar type Agent::"agent-1"
OData action "read"   → Cedar Action::"read"
Entity type "Order"   → Cedar resource Order::"order-123"
```

**Policy example:**
```cedar
// Admins can do anything
permit(principal is Admin, action, resource);

// Agents can deploy only when parent agent status is OK
permit(principal is Agent, action == Action::"canary_deploy", resource is DeployWorkflow)
  when { context.ctx_parent_agent_status == "canary_ok" };
```

**Integration gate** (trait, not concrete impl):
- `authorize_http_call(domain, method, url, ctx)` → Allow/Deny
- `authorize_secret_access(secret_key, ctx)` → Allow/Deny
- Concrete `CedarIntegrationAuthzGate` lives in `temper-server` (not moving yet)

### Dependencies

```toml
[dependencies]
temper-runtime = { ... }     # ← FIRST intra-workspace dep
cedar-policy = "4"           # ← NEW to dd-source (crate-local)
serde = "1"                  # ← crate-local
serde_json = { workspace }   # ← already in dd-source workspace
thiserror = "2"              # ← crate-local
uuid = "1"                   # ← crate-local
```

**No dev-dependencies.** All tests are inline `#[cfg(test)]`.

### Who depends on temper-authz?

```
temper-jit, temper-mcp, temper-odata, temper-platform, temper-server
```

## Pre-Flight Checks

- [x] `cargo test -p temper-authz` passes in origin repo
- [x] 1 internal dep only (`temper-runtime`, already in dd-source)
- [x] `cedar-policy` v4 is a pure Rust crate — no C deps, no build.rs, no system libs
- [x] No test fixtures needed (all tests are inline with string literals)
- [x] No nightly features required
- [x] No `include_str!` or `include_bytes!` — no compile_data needed

## Steps

### Step 1: Register in root Cargo.toml

Add to `[workspace] members`:
```toml
"domains/odp/temper/crates/temper-authz",
```

Add to `[workspace.dependencies]`:
```toml
temper-authz = { path = "domains/odp/temper/crates/temper-authz" }
```

### Step 2: Copy source

```bash
mkdir -p ~/dd/dd-source/domains/odp/temper/crates/temper-authz/src
rsync -av ~/go/src/github.com/DataDog/temper/crates/temper-authz/src/ \
          ~/dd/dd-source/domains/odp/temper/crates/temper-authz/src/
```

### Step 3: Write Cargo.toml

```toml
[package]
name = "temper-authz"
version.workspace = true
edition.workspace = true
license = "MIT OR Apache-2.0"
description = "Cedar-based authorization engine for Temper entity services"

[dependencies]
temper-runtime = { path = "../temper-runtime" }
cedar-policy = "4"
serde = { version = "1", features = ["derive"] }
serde_json = { workspace = true }
thiserror = "2"
uuid = { version = "1", features = ["v4", "v7", "serde"] }
```

Dep strategy (**CRITICAL — CI broke when violated on PR #379148**):
- **Already in dd-source workspace** (`serde_json`, `tokio`, `syn`, `anyhow`, `tracing`) → `{ workspace = true }`
- **Temper-only deps** (`cedar-policy`, `serde`, `thiserror`, `uuid`) → pin version **directly in this Cargo.toml**, NEVER add to root `[workspace.dependencies]` — adding/overwriting workspace deps breaks other services in the monorepo
- **Intra-workspace** (`temper-runtime`) → `{ workspace = true }` in crate Cargo.toml (points to root entry `temper-runtime = { path = "..." }`). dd-source manifest test **rejects raw path deps** — must go through workspace.

### Step 4: Write BUILD.bazel

```python
load("//rules/rust:defs.bzl", "dd_cargo_dot_toml", "dd_rust_library", "dd_rust_test")
load("//third_party/crates:defs.bzl", "all_crate_deps")

dd_cargo_dot_toml(src = "Cargo.toml")

dd_rust_library(
    name = "temper-authz",
    srcs = glob(["src/**/*.rs"]),
    crate_name = "temper_authz",
    visibility = ["//domains/odp:__subpackages__"],
    deps = all_crate_deps(normal = True) + [
        "//domains/odp/temper/crates/temper-runtime",
    ],
)

dd_rust_test(
    name = "test",
    crate = ":temper-authz",
    deps = all_crate_deps(normal_dev = True),
)
```

Note: `temper-runtime` is a **Bazel dep** (not from `all_crate_deps`) since it's
an intra-workspace crate, not a third-party crate.

### Step 5: Regenerate vendor

```bash
cd ~/dd/dd-source
bzl run //third_party:crates_vendor
```

This will pull `cedar-policy v4` and its transitive deps into `third_party/crates/defs.bzl`.

### Step 6: Build and test

```bash
bzl build //domains/odp/temper/...
bzl test //domains/odp/temper/...
```

Expected: 17+ test targets pass (macros + runtime + spec + authz).

### Step 7: Fix issues

Likely issues:
- **Rustdoc links**: Check for any `[`super::...`]` or `[`crate::...`]` links that may break
- **cedar-policy compilation**: v4 is straightforward but may pull heavy transitive deps
  (lalrpop, etc.) — crates_vendor will handle it

### Step 8: Commit and push

Amend onto the existing spec branch (stacked PR), or create a new stacked branch
`gbaldoni/no-ticket/temper-authz-to-dd-source` based on the spec branch.

## Risk Assessment

| Risk | Likelihood | Mitigation |
|---|---|---|
| cedar-policy pulls large transitive dep tree | Medium | crates_vendor handles it; cedar v4 is well-maintained |
| Intra-workspace dep on temper-runtime breaks Bazel | Low | Pattern is well-established (explicit Bazel dep + path in Cargo.toml) |
| cedar-policy conflicts with existing dd-source deps | Low | cedar-policy is not used anywhere else in dd-source |
| Rustdoc link errors | Low | Only 5 small files, easy to audit |
