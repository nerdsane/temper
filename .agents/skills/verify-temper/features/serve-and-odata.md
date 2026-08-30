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
Read an entity set named in the metadata; dispatch an action via `POST /tdata/<Set>('<id>')/Temper.<Action>` with `X-Tenant-Id`.

## What proves it
Health 200 with a live process; metadata is CSDL XML listing the bootstrapped entity types; an entity read returns `@odata.context`. A dispatch is proven by reading the entity back and seeing the state move - a 200 on dispatch alone is not a transition.

## Gotchas
- The serve log at bootstrap lists every spec that loaded; a missing entity set means its spec failed the cascade - read the log, not the route.
- Never drive this file with a bare `cargo run -p temper-cli -- serve`. Isolate exactly as SKILL.md Launch (`unset TURSO_PLATFORM_URL`, per-worktree `TURSO_URL=file:$PWD/.scratch/temper.db`, `--storage turso`) or you share/corrupt another session's db (ARN-435).
