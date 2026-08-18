# Agent Runtime PoC — Local Setup Guide

This guide walks you through running the PoC end-to-end on your machine
using the local sandbox (no Tensorlake API key needed).

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

## Step 1: Build the WASM modules

The WASM modules are the integration layer between Temper's state machine
and the sandbox/LLM. They must be built before starting the server.

```bash
cd os-apps/temper-agent/wasm
./build.sh
```

This compiles all 14 modules (including the new `sandbox_destroyer`)
to `target/wasm32-unknown-unknown/release/*.wasm`.

You should see output like:
```
Building llm_caller...
  -> llm_caller built successfully
Building tool_runner...
  -> tool_runner built successfully
...
All WASM modules built.
```

## Step 2: Start the Temper server

From the repo root:

```bash
# Set your Anthropic API key (required for the LLM to work)
export ANTHROPIC_API_KEY="sk-ant-..."

# Start the server with the temper-agent app installed
# The server auto-starts a local sandbox on port 3010
cargo run -p temper-cli -- serve \
  --port 3000 \
  --app temper-agent \
  --no-observe
```

What happens at startup:
- Temper loads the `temper-agent` app (IOA spec + CSDL + Cedar policies)
- It installs `temper-fs` first (declared dependency)
- It auto-starts the local sandbox at `http://127.0.0.1:3010`
- It seeds `anthropic_api_key`, `temper_api_url`, and `sandbox_url`
  into the secrets vault from environment variables
- The `/v1/agent-runs` API is available at `http://localhost:3000/v1`

You should see:
```
  Secrets vault: configured
  Local sandbox: http://127.0.0.1:3010 (auto-started)
Starting Temper platform server...
  Temper Data API: http://localhost:3000/tdata
  App: default (temper-agent)
```

## Step 3: Verify the server is running

```bash
# Health check
curl http://localhost:3000/tdata

# Verify the agent-run API is mounted
curl -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{"prompt": "hello"}'
```

You should get a `202 Accepted` with a `runId`.

## Step 4: Prepare the fixture repo in the sandbox

The local sandbox uses `/tmp/temper-sandbox` as its workdir. The agent
expects files at `/workspace` inside the sandbox. Copy the fixture:

```bash
# Ensure /workspace exists
mkdir -p /workspace

# Copy the fixture repo
cp -r test-fixtures/agent-runtime-fixture/* /workspace/

# Initialize git in /workspace (the agent needs git for diffs)
cd /workspace
git init
git add -A
git commit -m "fixture: calculator with deliberate divide bug"
cd -  # back to repo root
```

## Step 5: Create an agent run

```bash
curl -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{
    "prompt": "Fix the failing test in tests/test_calculator.py. The divide function in src/calculator.py has a bug — it returns a*b instead of a/b. Fix it, run the tests to verify, then show the git diff.",
    "model": "claude-sonnet-4-20250514",
    "tools": ["read", "write", "edit", "bash"],
    "sandbox_provider": "local",
    "workdir": "/workspace",
    "max_turns": "12"
  }'
```

Response:
```json
{
  "run_id": "run_abc123...",
  "status": "Provisioning"
}
```

## Step 6: Poll for status

```bash
# Replace run_abc123... with your actual run_id
RUN_ID="run_abc123..."

curl http://localhost:3000/v1/agent-runs/$RUN_ID \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin"
```

Response (while running):
```json
{
  "run_id": "run_abc123...",
  "status": "Executing",
  "turn": 3,
  "sandbox_id": "static-sandbox",
  "trace_id": "abcdef123456..."
}
```

Response (when done):
```json
{
  "run_id": "run_abc123...",
  "status": "Completed",
  "turn": 5,
  "sandbox_id": "static-sandbox",
  "trace_id": "abcdef123456...",
  "result": "I fixed the divide function..."
}
```

## Step 7: Verify the fix

```bash
# Check the file was fixed
cat /workspace/src/calculator.py | grep "def divide" -A2

# Check the test passes
cd /workspace && python -m pytest tests/ -v && cd -

# Check the git diff
cd /workspace && git diff HEAD && cd -
```

## Step 8: Steer a run (mid-execution)

Create a new run and immediately steer it:

```bash
# Create run
RUN_ID=$(curl -sf -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{
    "prompt": "Read src/calculator.py and explain what it does.",
    "tools": ["read", "bash"],
    "workdir": "/workspace",
    "max_turns": "5"
  }' | python3 -c "import sys,json; print(json.load(sys.stdin)['run_id'])")

# Steer it (inject a message for the next turn)
curl -X POST http://localhost:3000/v1/agent-runs/$RUN_ID/steer \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{"message": "Also mention the bug in the divide function."}'
```

## Step 9: Cancel a run

```bash
curl -X POST http://localhost:3000/v1/agent-runs/$RUN_ID/cancel \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin"
```

Response:
```json
{
  "run_id": "run_abc123...",
  "status": "Cancelled"
}
```

## Step 10: Run the full E2E test suite

```bash
bash os-apps/temper-agent/tests/agent_runtime_m1_e2e.sh
```

This runs all 9 tests:
1. Create agent run
2. Poll for completion
3. Verify durable state via OData
4. Verify fixture was fixed
5. Test Steer
6. Test Cancel
7. Idempotent tool callback retry (verifies `last_tool_batch_id`)
8. Unauthorized request denied by policy
9. Trace ID returned for Datadog correlation

## Using Tensorlake instead of local sandbox

When you have a Tensorlake API key:

```bash
# Set the key as a secret
curl -X POST http://localhost:3000/api/secrets \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{"key": "tensorlake_api_key", "value": "tlk_..."}'

# Create a run that uses Tensorlake
curl -X POST http://localhost:3000/v1/agent-runs \
  -H "content-type: application/json" \
  -H "x-tenant-id: default" \
  -H "x-temper-principal-kind: admin" \
  -d '{
    "prompt": "Fix the failing test and show the diff.",
    "sandbox_provider": "tensorlake",
    "sandbox_image": "tensorlake/ubuntu",
    "tools": ["read", "write", "edit", "bash"],
    "max_turns": "12"
  }'
```

Everything else (polling, steering, cancelling, tracing) works the same.

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
the server. The server should auto-upload them from the `os-apps/temper-agent/wasm/`
directory at startup.

### "No transition table for TemperAgent" error
The `temper-agent` app wasn't installed. Make sure you started the
server with `--app temper-agent`.

### Port conflicts
The server uses port 3000 and auto-starts the sandbox on port 3010.
If those are in use, specify a different port:
```bash
cargo run -p temper-cli -- serve --port 3001 --app temper-agent --no-observe
```
The sandbox will auto-start on port 3011.
