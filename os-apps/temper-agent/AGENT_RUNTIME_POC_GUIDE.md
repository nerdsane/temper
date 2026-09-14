# Agent Runtime PoC — Local Setup Guide

This guide walks you through running the PoC end-to-end on your machine
using either the local sandbox or Tensorlake.

## Prerequisites

1. **Rust toolchain** with `wasm32-unknown-unknown` target:
   ```bash
   rustup target add wasm32-unknown-unknown
   ```

2. **Python 3** (for the local sandbox server)

3. **An Anthropic API key** — the agent needs an LLM to think.
   Get one at https://console.anthropic.com/

4. **PostgreSQL** (optional — Turso/libSQL is the default and works
   for local dev, but Postgres is recommended for the PoC).

5. **Tensorlake API key** (only for Tensorlake runs) and a **GitHub
   fine-grained PAT** with Contents:Read on the fixture repository
   `gabrik/agent-runtime-fixture`.

## Step 1: Build the WASM modules

```bash
cd os-apps/temper-agent/wasm
./build.sh
```

This compiles all 14 modules to `target/wasm32-unknown-unknown/release/*.wasm`.

## Step 2: Start the Temper server

From the repo root:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export TEMPER_API_KEY="temper-local-dev-0001"

RUST_BACKTRACE=1 \
RUST_LOG='info,temper_server::agent_runtime=debug,temper_server::state::dispatch::wasm=debug,temper_wasm=debug' \
cargo run -p temper-cli -- serve \
  --port 3000 \
  --app temper-agent \
  --no-observe
```

Wait for `Listening on http://0.0.0.0:3000`.

## Step 3: Store secrets for Tensorlake runs

All protected API calls require `Authorization: Bearer $TEMPER_API_KEY`.
Store the Tensorlake and GitHub credentials in the tenant vault:

```bash
# Tensorlake API key
curl -X PUT \
  "http://localhost:3000/api/tenants/default/secrets/tensorlake_api_key" \
  -H "authorization: Bearer $TEMPER_API_KEY" \
  -H "content-type: application/json" \
  -d "{\"value\":\"$TENSORLAKE_API_KEY\"}"

# GitHub PAT (fine-grained, Contents:Read on gabrik/agent-runtime-fixture)
curl -X PUT \
  "http://localhost:3000/api/tenants/default/secrets/github_token" \
  -H "authorization: Bearer $TEMPER_API_KEY" \
  -H "content-type: application/json" \
  -d "{\"value\":\"$GITHUB_TOKEN\"}"

# Tensorlake API URL (optional — defaults to https://api.tensorlake.ai)
curl -X PUT \
  "http://localhost:3000/api/tenants/default/secrets/tensorlake_api_url" \
  -H "authorization: Bearer $TEMPER_API_KEY" \
  -H "content-type: application/json" \
  -d "{\"value\":\"https://api.tensorlake.ai\"}"
```

## Step 4: Create an agent run (Tensorlake with private repo clone)

```bash
curl -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "authorization: Bearer $TEMPER_API_KEY" \
  -d '{
    "prompt": "Fix the failing test in tests/test_calculator.py. The divide function in src/calculator.py has a bug — it returns a*b instead of a/b. Fix it, run the tests to verify, then show the git diff.",
    "model": "claude-sonnet-4-20250514",
    "tools": ["read", "write", "edit", "bash"],
    "sandbox_provider": "tensorlake",
    "repo_url": "https://github.com/gabrik/agent-runtime-fixture",
    "repo_ref": "main",
    "max_turns": "15"
  }'
```

Response:
```json
{
  "run_id": "run_abc123...",
  "status": "Provisioning"
}
```

## Step 5: Poll for status

```bash
RUN_ID="run_abc123..."

curl http://localhost:3000/v1/agent-runs/$RUN_ID \
  -H "x-tenant-id: default" \
  -H "authorization: Bearer $TEMPER_API_KEY"
```

Response (when done):
```json
{
  "run_id": "run_abc123...",
  "status": "Completed",
  "turn": 5,
  "sandbox_id": "pxrnq9h7e5c71dkbprbvz",
  "result": "I fixed the divide function..."
}
```

## Step 6: Delete a terminal run and its sandbox

`DELETE /v1/agent-runs/{id}` is teardown-gated: it accepts only completed,
failed, or cancelled runs; begins provider sandbox deletion; and returns `202`
while cleanup is in progress. Poll the run until it is absent (`404`). A
provider teardown error leaves the run in `DeletionFailed`; repeat the same
request to retry rather than orphaning the sandbox.

```bash
curl -X DELETE http://localhost:3000/v1/agent-runs/$RUN_ID \
  -H "x-tenant-id: default" \
  -H "authorization: Bearer $TEMPER_API_KEY"
```

Response while teardown is in progress:

```json
{
  "run_id": "run_abc123...",
  "status": "Deleting"
}
```

Repeat the same `GET` until it returns `404`. Repeat `DELETE` to retry
if the run is in `DeletionFailed`.

## Step 7: Run the full E2E test suite

```bash
bash os-apps/temper-agent/tests/agent_runtime_m1_e2e.sh
```

## Using the local sandbox instead of Tensorlake

For local-only testing (no Tensorlake API key needed):

```bash
# Copy the fixture into the local sandbox workdir
mkdir -p /tmp/temper-sandbox/workspace
cp -r test-fixtures/agent-runtime-fixture/* /tmp/temper-sandbox/workspace/
cd /tmp/temper-sandbox/workspace && git init && git add -A && \
  git commit -m "fixture" && cd -

# Create a local run
curl -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "authorization: Bearer $TEMPER_API_KEY" \
  -d '{
    "prompt": "Fix the failing test and show the diff.",
    "sandbox_provider": "local",
    "workdir": "/tmp/temper-sandbox/workspace",
    "tools": ["read", "write", "edit", "bash"],
    "max_turns": "12"
  }'
```

## Troubleshooting

### "sandbox_url is empty" error
The server didn't auto-start the local sandbox. Start it manually:
```bash
python3 os-apps/temper-agent/sandbox/local_sandbox.py --port 3010 --workdir /tmp/temper-sandbox
```

### "anthropic_api_key not set" error
The `ANTHROPIC_API_KEY` environment variable wasn't set when the server
started. Stop the server, set it, and restart.

### Agent stays in "Provisioning" forever
Check the server logs for WASM module errors. The most common issue is
that the WASM modules weren't built (Step 1) or weren't uploaded to
the server.

### "git clone failed (exit 128): Invalid username or token"
The GitHub PAT stored in the vault is invalid, expired, or lacks
Contents:Read on the target repository. Regenerate the token, store it
via the `PUT /api/tenants/default/secrets/github_token` endpoint, and
retry.

### "No transition table for TemperAgent" error
The `temper-agent` app wasn't installed. Make sure you started the
server with `--app temper-agent`.

### Port conflicts
The server uses port 3000 and auto-starts the sandbox on port 3010.
If those are in use, specify a different port:
```bash
cargo run -p temper-cli -- serve --port 3001 --app temper-agent --no-observe
```
