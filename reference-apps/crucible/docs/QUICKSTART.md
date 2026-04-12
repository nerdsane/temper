# Crucible — Quickstart

Get a working agent conversation in under 5 minutes.

---

## Prerequisites

```bash
# Build the CLI
cargo build -p crucible-reference
cargo build -p temper-cli
```

You need an LLM API key from any OpenAI-compatible provider.
This guide uses [Fireworks](https://fireworks.ai) but OpenAI,
Together, Groq, or any compatible endpoint works.

---

## 1. Start the server

```bash
export TEMPER_VAULT_KEY=$(python3 -c "import os,base64; print(base64.b64encode(os.urandom(32)).decode())")
./target/debug/temper serve \
    --port 3000 \
    --specs-dir reference-apps/crucible/specs \
    --tenant crucible \
    --no-observe
```

## 2. Store your LLM key

```bash
curl -X PUT \
  -H "Content-Type: application/json" \
  -H "X-Temper-Principal-Kind: admin" \
  "http://127.0.0.1:3000/api/tenants/crucible/secrets/llm_api_key" \
  -d '{"value":"YOUR_API_KEY_HERE"}'
```

## 3. Seed entities

```bash
crucible-chat seed \
    --server http://127.0.0.1:3000 --tenant crucible \
    --provider openai \
    --openai-api-key "$OPENAI_API_KEY" \
    --openai-base-url "https://api.fireworks.ai/inference/v1" \
    --model "accounts/fireworks/routers/kimi-k2p5-turbo"
```

This creates an Environment, ManagedAgent, and Session, printing:

```
environment_id=env-chat-a1b2c3d4
agent_id=agt-chat-e5f6g7h8
session_id=sess-chat-i9j0k1l2
```

## 4. Send a message

```bash
crucible-chat send <session-id> "What is the capital of Japan?"
```

```
Message sent (seq=0). Use `watch` to drive the agent loop.
```

## 5. Start the watcher

In a second terminal:

```bash
crucible-chat watch <session-id> --poll-interval 1 \
    --provider openai \
    --openai-api-key "$OPENAI_API_KEY" \
    --openai-base-url "https://api.fireworks.ai/inference/v1"
```

Within 1 second:

```
[watch] Pending user message detected. Running agent turn...
Agent: The capital of Japan is Tokyo.
[watch] Turn complete. input_tokens=42 output_tokens=15
```

## 6. Multi-turn conversation

Back in the first terminal:

```bash
crucible-chat send <session-id> "And what is its population?"
```

The watcher picks it up and responds:

```
Agent: Tokyo's population is approximately 14 million.
```

The agent sees full conversation history — each turn re-reads the
event feed. No in-process state survives between turns.

## 7. Interrupt

```bash
crucible-chat interrupt <session-id> --message "Stop, tell me about Osaka instead"
```

## 8. Read the event feed

```bash
curl -H "X-Tenant-Id: crucible" \
  "http://127.0.0.1:3000/tdata/SessionEvents?\$filter=SessionId%20eq%20'<session-id>'&\$orderby=Sequence%20asc"
```

---

## Modal Sandbox (isolated tool execution)

For tool calls to execute inside a remote container instead of on
your machine:

### Extra prerequisites

```bash
pip3 install fastapi uvicorn httpx modal
python3 -m modal setup   # one-time auth
```

### Start the tool server

```bash
TEMPER_API_URL=http://127.0.0.1:3000 TEMPER_TENANT=crucible \
  python3 -m uvicorn reference-apps.crucible.modal_bridge.server:app --port 3100
```

### Create a Modal environment

```bash
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/Environments" \
  -d '{"id":"env-modal","Name":"modal-sandbox","Status":"Active",
       "ConfigType":"Modal","NetworkingType":"Unrestricted",
       "ModalImage":"python:3.12-slim","ModalCpu":1.0,"ModalMemory":2048,
       "ModalTimeout":300,"ModalWorkdir":"/workspace",
       "CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}'
```

The sandbox is provisioned lazily on the first tool call — no
explicit provisioning step needed.

---

## Cron Scheduling (background triggers)

Run an agent on a schedule without any human interaction:

### Build and upload WASM modules

```bash
rustup target add wasm32-unknown-unknown   # one-time
cd reference-apps/crucible/wasm && ./build.sh --upload
```

### Create a schedule

```bash
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/SessionSchedules" \
  -d '{"id":"sched-01","SessionId":"<session-id>",
       "CronExpression":"0 9 * * 1-5",
       "MessageTemplate":"Generate a standup summary for {{now}}.",
       "Status":"Draft",
       "CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}'

# Activate
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/SessionSchedules('sched-01')/Temper.Crucible.ActivateSchedule" \
  -d '{}'
```

### Start the scheduler heartbeat

```bash
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/CrucibleSchedulers" \
  -d '{"id":"cs-01","Status":"Idle","HeartbeatIntervalSeconds":30,
       "CreatedAt":"2026-04-12T00:00:00Z"}'

curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/CrucibleSchedulers('cs-01')/Temper.Crucible.Start" \
  -d '{"heartbeat_interval_seconds":"30"}'
```

The scheduler posts `user.message` events at the scheduled time.
The `watch` process picks them up and drives agent turns. No
external cron daemon needed — everything runs inside Temper.

---

## Adding Memory

Attach a memory store so the agent builds up durable knowledge:

```bash
# Create a memory store
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/MemoryStores" \
  -d '{"id":"ms-01","Name":"project-context",
       "Description":"Team conventions","Status":"Active",
       "CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}'

# Pre-populate
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/Memories" \
  -d '{"id":"mem-01","MemoryStoreId":"ms-01",
       "Path":"/preferences/style.md",
       "Content":"Always use TypeScript, 2-space indentation.",
       "SizeBytes":44,
       "CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}'

# Attach to session
curl -X POST -H "X-Tenant-Id: crucible" -H "Content-Type: application/json" \
  "http://127.0.0.1:3000/tdata/SessionResources" \
  -d '{"id":"sr-01","SessionId":"<session-id>",
       "Kind":"memory_store","MemoryStoreId":"ms-01",
       "Access":"read_write",
       "Prompt":"Check team preferences before writing code.",
       "CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}'
```

---

## What's next

- [Architecture deep-dive](ARCHITECTURE.md) — how the sidecar, OData, and WASM scheduling work under the hood
- [Known gaps](KNOWN_GAPS.md) — what Anthropic supports that Crucible doesn't yet
- [Overview](OVERVIEW.md) — all concepts and entity relationships
