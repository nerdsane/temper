# DSF Deploy

Shared Deep Sci-Fi deploy sequence. Howl and TemperPaw both drive this. Hands
run on TensorLake sandbox `dsf`.

This is a catalog tool, not a Railway change and not a Galley publish. Install
it into the tenant that will run the walk.

## Entity Types

### DeployRun

One deploy of one already-named target.

**States**: Idle → Checking → Ready → Deploying → Verifying → Healthy

Failure and cancel sit beside that walk: `MarkFailed` from Checking,
Deploying, or Verifying; `Cancel` from any non-terminal work state;
`ResumeOnComputer` from Failed back to Checking.

**State vars**: `has_target`, `health_ok`, `deploy_recorded` (all bool, start false)

**Key actions**:
- **Arm**: Name the target (`Service`, `Environment`, `ComputerName`). Stays Idle. Sets `has_target`.
- **ProbeHealth**: Idle, Checking, or Failed → Checking. Requires `has_target`.
- **MarkReady**: Checking → Ready. Sets `health_ok`.
- **StartDeploy**: Ready → Deploying.
- **RecordDeploy**: Stays Deploying. Records `DeployId`. Sets `deploy_recorded`.
- **StartVerify**: Deploying → Verifying. Requires `deploy_recorded`.
- **MarkHealthy**: Verifying → Healthy (final).
- **MarkFailed**: Checking, Deploying, or Verifying → Failed.
- **Cancel**: Idle, Checking, Ready, Deploying, or Verifying → Cancelled (final).
- **ResumeOnComputer**: Failed → Checking.

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

## Sandbox `dsf`

The live temper-agent provision path is the WASM guest
`os-apps/temper-agent/wasm/sandbox_provisioner/src/lib.rs`, not
`local_sandbox.py`. That guest either uses a static `sandbox_url` or creates an
ephemeral E2B sandbox. It does not read `TEMPER_SANDBOX_NAME` and has no
TensorLake named-sandbox resume.

Until a gated hook lands in `provision_sandbox()` (after the static URL return,
before `POST /sandboxes`), connect `dsf` by setting `sandbox_url` on Configure
or integration config. Do not change the WASM guest from this app.
