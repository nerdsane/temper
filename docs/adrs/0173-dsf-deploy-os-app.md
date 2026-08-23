# ADR-0173: Shared Deep Sci-Fi Deploy Sequence OS App

- Status: Proposed
- Date: 2026-08-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0031: Temper-native agent
  - `os-apps/project-management/` (shape this app follows, smaller)
  - `os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs` (provision path)

## Context

Howl and TemperPaw both need the same Deep Sci-Fi deploy walk: arm a target,
probe health, deploy, record the deploy id, verify, then mark healthy or fail.
That walk is a state machine. It belongs in the kernel catalog as a shared OS
app, not as a one-off helper in either agent repo.

Sandbox hands for that work run on the TensorLake sandbox named `dsf`. The
hypothesis that temper-agent can connect or resume that named sandbox from
`TEMPER_SANDBOX_NAME` must be verified against the actual provision path
before any host or guest change.

## Decision

### Sub-Decision 1: Ship `os-apps/dsf-deploy`

Add a catalog app named `dsf-deploy` with one entity, `DeployRun`. Follow the
project-management bundle layout (`app.toml`, `APP.md`, IOA, CSDL, Cedar) and
keep it smaller: one machine, no extra entities, no WASM, no startup install.

`startup_install` stays at the default `manual`. This app is installable. It is
not part of the core boot surface.

**Why this approach**: the catalog already discovers any directory with
`app.toml` + `APP.md`. Install already loads root-level `*.ioa.toml`,
`model.csdl.xml`, and `policies/*.cedar`. No kernel loader change is required.

### Sub-Decision 2: Do not change the provisioner in this PR

Verified in code (not hypothesized):

1. `os-apps/temper-agent/sandbox/local_sandbox.py` is the local process helper.
   Out of scope.
2. The live provision path is the WASM guest
   `os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs`, triggered by
   `TemperAgent.Provision` in `os-apps/temper-agent/specs/temper_agent.ioa.toml`.
3. `provision_sandbox()` has two priorities today:
   - static `sandbox_url` from entity fields, integration config, or trigger
     params
   - otherwise `POST {e2b_api_url}/sandboxes` (ephemeral E2B create)
4. There is no TensorLake client, no named-sandbox resume, and no read of
   `TEMPER_SANDBOX_NAME` anywhere on this path. WASM guests do not see host
   environment variables unless the host injects them into `ctx.config`.

Changing that guest is not safe in this PR: it requires a rebuilt
`wasm32-unknown-unknown` artifact, and a mistake breaks every existing
ephemeral `Provision`.

**Smallest later hook** (do not land here):

- File: `os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs`
- Function: `provision_sandbox()`
- Insert after the static `sandbox_url` return (today ~L148) and before the
  ephemeral E2B `POST /sandboxes` (today ~L159).
- Gate: if `ctx.config` has a non-empty sandbox name, connect/resume that
  named sandbox; if empty, keep current create.
- Host injection (second, smaller file): add the name to
  `[action.triggers.config]` on `Provision` in
  `os-apps/temper-agent/specs/temper_agent.ioa.toml`, sourced from
  `TEMPER_SANDBOX_NAME` (default empty).

Until that hook exists, operators can still point a run at `dsf` by setting
`sandbox_url` on Configure / integration config. That reuses priority 1 and
does not change Provision.

### Sub-Decision 3: Non-goals stay out of this PR

Do not change Railway. Do not publish Galley. Do not rewrite other OS apps.

## Consequences

### Positive
- Howl and TemperPaw can install one shared deploy machine.
- The catalog loads the app with the same files sibling apps already require.

### Negative
- Named sandbox `dsf` is not auto-connected. Hands still need an explicit
  `sandbox_url` or a later WASM hook.

### Risks
- If the IOA walk is too strict, agents will 409. The machine is the
  contract; walk the states instead of adding shortcut actions.

## Non-Goals

- TensorLake or E2B named-sandbox resume
- Railway or Galley changes
- Changes to `temper-agent` WASM artifacts
- Auto-install at platform boot
