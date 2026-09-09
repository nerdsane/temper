# ADR-0173: Shared Deep Sci-Fi Deploy and Investigate OS Apps

- Status: Proposed
- Date: 2026-08-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0031: Temper-native agent
  - ADR-0046: Inline WASM triggers
  - `os-apps/project-management/` (bundle shape this pair follows, smaller)
  - `os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs` (provision path)

## Context

Howl and TemperPaw both need the same Deep Sci-Fi deploy walk and the same
Datadog investigation walk. Those walks are state machines, but Temper apps
are not IOA-only: they also carry guest scripts as WASM. An IOA without a
module that does real work is not enough to ship.

Sandbox hands for that work run on the TensorLake named sandbox `dsf`
(also called dd comp). The live temper-agent provision path is the WASM
guest `sandbox_provisioner`. Today that guest either uses a static
`sandbox_url` or creates an ephemeral E2B sandbox. It has no TensorLake
client and does not read `TEMPER_SANDBOX_NAME` / `TEMPER_SANDBOX_URL`.
WASM guests do not see host environment variables unless the host injects
them into `ctx.config`.

The `dsf` sandbox must be stocked with Datadog access and the official
`pup` CLI. Secret *values* must never be invented, printed, or written
by this change.

## Decision

### Sub-Decision 1: Ship `os-apps/dsf-deploy` with IOA and WASM

Catalog app `dsf-deploy`, entity `DeployRun`. Bundle layout matches
project-management (`app.toml`, `APP.md`, IOA, CSDL, Cedar) plus a guest
module `dsf_deploy`.

The machine is unchanged in spirit: Arm → ProbeHealth → MarkReady →
StartDeploy → RecordDeploy → StartVerify → MarkHealthy, with Failed /
Cancelled / ResumeOnComputer beside that walk.

`ProbeHealth`, `ResumeOnComputer`, `StartDeploy`, and `StartVerify`
trigger `dsf_deploy`. The module performs real HTTP:

- Probe / resume: `GET` `health_url` (or `sandbox_url` + `/health`)
- Deploy: `POST` `deploy_url` (or `sandbox_url` + `/deploy`) and return
  `DeployId`
- Verify: `GET` `verify_url`, else the same health URL

Missing URLs fail closed with operator instructions. The module does not
invent a Railway or TensorLake API and does not log secret values.

`startup_install` stays `manual`.

**Why this approach**: Howl and TemperPaw need one shared walk *and* the
hands that execute the probe/deploy/verify steps.

### Sub-Decision 2: Ship sibling app `os-apps/dsf-investigate`

Same pattern: IOA + WASM + CSDL + Cedar. Entity `Investigation`. Stored
workflow: Arm → StartGather → RecordFindings → MarkReady, with Failed /
Cancelled / Resume.

`StartGather` and `Resume` trigger `dsf_investigate`. The module calls
the Datadog HTTP API when credentials are present in config (`DD_SITE`,
plus `DD_ACCESS_TOKEN` or `DD_API_KEY` + `DD_APP_KEY`). It never prints
those values. If credentials are missing, it fails closed and tells the
operator to stock sandbox `dsf` with `pup` and the named env vars.

**Why this approach**: investigations are a second shared tool on the
same computer, not a second state inside DeployRun.

### Sub-Decision 3: Gated named-sandbox path in `sandbox_provisioner`

Land the hook that the first draft of this ADR deferred.

Priority in `provision_sandbox()`:

1. Usable static `sandbox_url` (entity fields, config, trigger params).
   Unresolved `{secret:...}` values are treated as unset so they do not
   become a URL.
2. Named sandbox from `temper_sandbox_url` / `TEMPER_SANDBOX_URL`.
   Sandbox id is `temper_sandbox_name` / `TEMPER_SANDBOX_NAME` when set,
   else `named-sandbox`.
3. If a name is set and the URL is empty: **fail closed**. Do not create
   an ephemeral E2B sandbox when the operator asked for `dsf`.
4. Else existing E2B `POST /sandboxes`.

There is still no TensorLake create/resume client. Connect means "use
this URL". Empty name and empty URL keep today's E2B path.

Host injection (WASM cannot read env):

- `os-apps/temper-agent/specs/temper_agent.ioa.toml` Provision trigger
  config adds `temper_sandbox_name` and `temper_sandbox_url` as
  `{secret:...}` templates.
- After secret resolution, `crates/temper-server/src/secrets/env_overlay.rs`
  overlays process env `TEMPER_SANDBOX_NAME` / `TEMPER_SANDBOX_URL` when
  those config keys are empty or unresolved. Datadog keys are overlaid
  the same way, but only when the trigger already declared them.

`std::env::var` is annotated `// determinism-ok`: production provision
config, not entity state.

**Why this approach**: smallest hook that connects `dsf` without
default-breaking ephemeral E2B.

### Sub-Decision 4: Stock script for sandbox `dsf`

`os-apps/dsf-deploy/scripts/stock_dsf_sandbox.sh` installs `pup` when
missing and checks that Datadog env *names* are present. It does not
write secrets, print secret values, bounce Railway, or publish Galley.

Official `pup` install (do not invent asset names):

- `brew tap datadog-labs/pack && brew install datadog-labs/pack/pup`
- `git clone https://github.com/DataDog/pup.git && cd pup && cargo build --release`
- Prebuilt binaries from https://github.com/DataDog/pup/releases/latest

Env names (values never printed):

- `DD_SITE` (default `datadoghq.com` if unset)
- `DD_ACCESS_TOKEN` (highest priority)
- or `DD_API_KEY` + `DD_APP_KEY`
- optional: `PUP_TRUST_SITE`, `DD_ORG`, `DD_TOKEN_STORAGE`
- Temper connect: `TEMPER_SANDBOX_NAME` (expected `dsf`), `TEMPER_SANDBOX_URL`
- TensorLake proxy: `TENSORLAKE_API_KEY` (never printed)

### Sub-Decision 5: `tool_runner` speaks TensorLake process+fs when named

Storing `sandbox_url` is not enough. The live TemperPaw door is
`os-apps/temper-agent/wasm/tool_runner`. It still spoke E2B
(`/v1/processes/run`, `/v1/fs/file`, envd `/files`) after the URL was
saved. TensorLake named sandbox `dsf` does not.

When `TEMPER_SANDBOX_NAME` is set or the URL host is
`*.sandbox.tensorlake.ai`, `tool_runner` uses the official TensorLake
proxy APIs and a Bearer from `tensorlake_api_key` /
`TENSORLAKE_API_KEY`:

- Exec: `POST {url}/api/v1/processes` with `{command, args, env, working_dir}`,
  then poll `GET .../processes/{pid}` and stdout/stderr
- Files: `GET`/`PUT` `{url}/api/v1/files?path=`

Empty name keeps the existing E2B / local path. There is still no
TensorLake create client. Resume is TensorLake's (`tl sbx resume dsf`).

Host overlay copies `TENSORLAKE_API_KEY` into `tensorlake_api_key` only
when that key is already declared on the trigger (same as Datadog).
Named-sandbox name/url may still insert if absent; those are not a
bearer. The value is never logged.

### Sub-Decision 6: Non-goals stay out

Do not bounce Railway. Do not publish Galley. Do not dump secrets. Do
not invent a TensorLake create/resume client.

## Consequences

### Positive
- Howl and TemperPaw can install shared deploy and investigate machines
  that actually run hands.
- Operators can point Provision at `dsf` without changing the E2B
  default.
- The stock script is a repeatable, secret-safe checklist.

### Negative
- Named sandbox connect still requires a URL. Name-only fails closed
  instead of guessing a TensorLake API.
- Host overlay is process-env, not a vault write.

### Risks
- If the IOA walk is too strict, agents will 409. Walk the states.
- If `TEMPER_SANDBOX_NAME=dsf` is set without a URL, Provision fails
  instead of silently creating E2B. That is intentional.

### DST Compliance
- `env_overlay` is production host config. Tests inject values; they do
  not read process env.
- Overlay uses `BTreeMap`. No wall clock, no thread spawn, no entity
  state mutation from env.

## Non-Goals

- TensorLake HTTP create/resume client
- Railway or Galley changes
- Auto-install at platform boot
- Printing or storing secret values
