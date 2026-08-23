# DSF Deploy

Shared Deep Sci-Fi deploy sequence. Howl and TemperPaw both drive this. Hands
run on TensorLake sandbox `dsf` (also called dd comp).

This is a catalog tool, not a Railway change and not a Galley publish. Install
it into the tenant that will run the walk.

Temper apps are not IOA-only. `DeployRun` is the walk; `dsf_deploy` WASM is
the hands (HTTP probe, deploy, verify).

## Entity Types

### DeployRun

One deploy of one already-named target.

**States**: Idle → Checking → Ready → Deploying → Verifying → Healthy

Failure and cancel sit beside that walk: `MarkFailed` from Checking,
Deploying, or Verifying; `Cancel` from any non-terminal work state;
`ResumeOnComputer` from Failed back to Checking (re-probes).

**State vars**: `has_target`, `health_ok`, `deploy_recorded` (all bool, start false)

**Key actions**:
- **Arm**: Name the target (`Service`, `Environment`, `ComputerName`). Stays Idle. Sets `has_target`.
- **ProbeHealth**: Idle, Checking, or Failed → Checking. Requires `has_target`. Runs `dsf_deploy` (`GET` `health_url` or `sandbox_url/health`). Success → MarkReady. Failure → MarkFailed.
- **MarkReady**: Checking → Ready. Sets `health_ok`.
- **StartDeploy**: Ready → Deploying. Runs `dsf_deploy` (`POST` `deploy_url` or `sandbox_url/deploy`). Success → RecordDeploy with `DeployId`.
- **RecordDeploy**: Stays Deploying. Records `DeployId`. Sets `deploy_recorded`.
- **StartVerify**: Deploying → Verifying. Requires `deploy_recorded`. Runs `dsf_deploy` (`GET` `verify_url` or health). Success → MarkHealthy.
- **MarkHealthy**: Verifying → Healthy (final).
- **MarkFailed**: Checking, Deploying, or Verifying → Failed.
- **Cancel**: Idle, Checking, Ready, Deploying, or Verifying → Cancelled (final).
- **ResumeOnComputer**: Failed → Checking. Re-probes.

**Invariants**:
- Cancelled and Healthy admit no further transitions.
- Checking, Ready, Deploying, Verifying, and Healthy require `has_target`.

Walk the states. Do not add a shortcut that jumps Idle → Healthy.

## Setup

```
temper.install_app("dsf-deploy")
```

Startup install is manual. The app is catalog-visible after this directory
ships; it is not part of the core boot surface.

Build the guest (requires `wasm32-unknown-unknown`):

```
os-apps/dsf-deploy/wasm/build.sh
```

## Sandbox `dsf`

The live temper-agent provision path is the WASM guest
`os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs`, not
`local_sandbox.py`.

Priority after this change:

1. Usable static `sandbox_url` (Configure / config / trigger). Unresolved
   `{secret:...}` templates are treated as unset.
2. Named sandbox: `TEMPER_SANDBOX_URL` / `temper_sandbox_url`. Id is
   `TEMPER_SANDBOX_NAME` / `temper_sandbox_name` when set (expected `dsf`).
3. If a name is set and the URL is empty: fail closed. Do not create E2B.
4. Else ephemeral E2B `POST /sandboxes` (unchanged default).

WASM cannot read host env. The Temper host overlays
`TEMPER_SANDBOX_NAME` and `TEMPER_SANDBOX_URL` into provisioner config
after secret resolution. There is still no TensorLake create client;
connect means "use this URL".

Do not bounce Railway. Do not publish Galley.

## Stock `dsf` (Datadog + pup)

Run on the sandbox (or any host that should carry the same tools). The
script installs `pup` if missing and checks env *presence*. It does not
write secrets or print secret values.

```
os-apps/dsf-deploy/scripts/stock_dsf_sandbox.sh
```

Optional: `STOCK_DSF_BUILD_PUP=1` builds pup from source when Homebrew
is unavailable.

### pup install (official, do not invent assets)

- Homebrew: `brew tap datadog-labs/pack && brew install datadog-labs/pack/pup`
- Source: `git clone https://github.com/DataDog/pup.git && cd pup && cargo build --release`
- Prebuilt binaries: https://github.com/DataDog/pup/releases/latest
- Docs: https://github.com/DataDog/pup and https://docs.datadoghq.com/cli/

### Env names (values never printed)

Datadog / pup (from the pup README):

- `DD_SITE` — Datadog site. Default `datadoghq.com` if unset.
- `DD_ACCESS_TOKEN` — bearer token; highest priority.
- `DD_API_KEY` + `DD_APP_KEY` — fallback when no access token.
- `PUP_TRUST_SITE` — optional; trust a non-Datadog site for one invocation.
- `DD_ORG` — optional named session.
- `DD_TOKEN_STORAGE` — optional; `keychain` or `file`.

Temper connect (host that runs Provision, not secret values):

- `TEMPER_SANDBOX_NAME` — expected `dsf`
- `TEMPER_SANDBOX_URL` — URL of the named TensorLake sandbox

Deploy guest (Temper trigger / secrets, not printed):

- `health_url`, `deploy_url`, `verify_url`, `sandbox_url`

Auth for pup on the sandbox is `DD_ACCESS_TOKEN`, or both `DD_API_KEY`
and `DD_APP_KEY`. Do not invent or dump those values.
