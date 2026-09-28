# DSF Investigate

Shared Deep Sci-Fi Datadog investigation sequence. Howl and TemperPaw both
drive this. Hands run on TensorLake sandbox `dsf` (also called dd comp).

This is a catalog tool, not a Railway change and not a Galley publish.
Sibling of `dsf-deploy`: same IOA + WASM pattern, stored investigation
workflow.

## Entity Types

### Investigation

One stored Datadog investigation of an already-named query.

**States**: Idle → Gathering → Analyzing → Ready

Failure and cancel sit beside that walk: `MarkFailed` from Gathering or
Analyzing; `Cancel` from Idle, Gathering, or Analyzing; `Resume` from
Failed back to Gathering (re-gathers).

**State vars**: `has_query`, `findings_recorded` (bool, start false)

**Key actions**:
- **Arm**: Name the query (`Service`, `Query`, `TimeRange`). Stays Idle. Sets `has_query`.
- **StartGather**: Idle or Failed → Gathering. Requires `has_query`. Runs `dsf_investigate` (Datadog HTTP). Success → RecordFindings.
- **RecordFindings**: Gathering → Analyzing. Records `FindingCount`. Sets `findings_recorded`.
- **MarkReady**: Analyzing → Ready (final). Requires `findings_recorded`.
- **MarkFailed**: Gathering or Analyzing → Failed.
- **Cancel**: Idle, Gathering, or Analyzing → Cancelled (final).
- **Resume**: Failed → Gathering. Re-gathers.

**Invariants**:
- Ready and Cancelled admit no further transitions.
- Gathering, Analyzing, and Ready require `has_query`.

Walk the states. Do not add a shortcut that jumps Idle → Ready.

## Setup

```
temper.install_app("dsf-investigate")
```

Startup install is manual.

Build the guest (requires `wasm32-unknown-unknown`):

```
os-apps/dsf-investigate/wasm/build.sh
```

## Datadog on sandbox `dsf`

`dsf_investigate` calls the Datadog HTTP API from the Temper host when
credentials are present in trigger config. Stock the named sandbox with
`pup` so hands on `dsf` can run the same investigations from the CLI.

Use the shared stock script (does not write or print secret values):

```
os-apps/dsf-deploy/scripts/stock_dsf_sandbox.sh
```

### pup install (official)

- `brew tap datadog-labs/pack && brew install datadog-labs/pack/pup`
- `git clone https://github.com/DataDog/pup.git && cd pup && cargo build --release`
- https://github.com/DataDog/pup/releases/latest

### Env names (values never printed)

- `DD_SITE` — default `datadoghq.com`
- `DD_ACCESS_TOKEN` — highest priority
- or `DD_API_KEY` + `DD_APP_KEY`
- optional: `PUP_TRUST_SITE`, `DD_ORG`, `DD_TOKEN_STORAGE`
- Temper connect: `TEMPER_SANDBOX_NAME` (expected `dsf`), `TEMPER_SANDBOX_URL`

The host overlays `DD_SITE` / `DD_ACCESS_TOKEN` / `DD_API_KEY` /
`DD_APP_KEY` into this module's config only when those keys are already
declared on the trigger. Unrelated WASM guests do not receive them.

Do not bounce Railway. Do not publish Galley. Do not dump secrets.
