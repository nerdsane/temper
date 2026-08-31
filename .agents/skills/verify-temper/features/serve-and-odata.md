# Serve and the OData surface

## Sub-features
Boot, health, CSDL metadata, entity-set reads, action dispatch.

## Driving it
Same isolate recipe as SKILL.md Launch — a bare `serve --port 3600` hits the shared `~/.local/share/temper/agents.db` (ARN-435). `TURSO_PLATFORM_URL` is checked **before** `TURSO_URL` (`serve/bootstrap.rs:59` vs `:90`); if it is set, the file URL is ignored.
```bash
unset TURSO_PLATFORM_URL
mkdir -p .scratch                                           # the turso file's dir must exist
TEMPER_API_KEY=local-verify \
TURSO_URL="file:$PWD/.scratch/temper.db" \
  cargo run -p temper-cli -- serve --port 3600 --storage turso   # capture PID
curl -sf http://localhost:3600/healthz
curl -sf -H 'X-Tenant-Id: default' 'http://localhost:3600/tdata/$metadata' | head -c 400   # CSDL XML
```
Class-A skip after SKILL Launch: `authz/edge.rs::is_public_kernel_request` admits GET `/tdata`, `/tdata/`, `/tdata/$metadata`, `/temper-client.js`, `/static/temper-client.js`, `/genesis`, `/genesis/`, `/genesis/*`, and GET|POST `/webhooks/`. Platform `/healthz` is unauthenticated one layer up (`build_platform_router`); it is not in that fn. Entity-set reads and dispatch are governed — they need the bearer **and** the tenant header. `GET /tdata/Plans` with `X-Tenant-Id` only → **401** (`TEMPER_API_KEY` is set). Set `KEY=$TEMPER_API_KEY`:
```bash
A=(-H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default")
curl -sS "http://localhost:3600/tdata/<Set>" "${A[@]}"
curl -sS -X POST "http://localhost:3600/tdata/<Set>('<id>')/Temper.<Action>" \
  "${A[@]}" -H "Content-Type: application/json" -d '{}'
```

## What proves it
Health 200 with a live process; metadata is CSDL XML listing the bootstrapped entity types; an entity read returns `@odata.context`. A dispatch is proven by reading the entity back and seeing the state move - a 200 on dispatch alone is not a transition.

## Gotchas
- The serve log at bootstrap lists every spec that loaded; a missing entity set means its spec failed the cascade - read the log, not the route.
- Never drive this file with a bare `cargo run -p temper-cli -- serve`. Isolate exactly as SKILL.md Launch (`unset TURSO_PLATFORM_URL`, per-worktree `TURSO_URL=file:$PWD/.scratch/temper.db`, `--storage turso`) or you share/corrupt another session's db (ARN-435).
