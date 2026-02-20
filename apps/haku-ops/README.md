# Haku Ops

Engineering pipeline management for Deep Sci-Fi, running on Temper.

## Tenant

This app runs under tenant `haku-ops`. All API requests must include `X-Tenant-Id: haku-ops` for proper multi-tenant isolation.

## Launch

```bash
# Start Temper with haku-ops as a named tenant
temper serve --app haku-ops=apps/haku-ops/specs --port 3001

# Seed historical data (first run only — hydration handles restarts)
bash apps/haku-ops/seed.sh

# Dashboard + proxy (serves UI on 8080, proxies /tdata to Temper on 3001)
python3 apps/haku-ops/serve.py

# Tunnel (optional, for remote access)
ssh -R 80:localhost:8080 localhost.run
```

## Architecture

- **Temper server** (port 3001): State machine backend with Postgres persistence
- **Python proxy** (port 8080): Serves dashboard HTML, proxies `/tdata` to Temper, bridges `/selection` for Haku heartbeat pickup
- **Tunnel**: localhost.run SSH tunnel for Rita's remote access

## Entities

| Entity | States | Purpose |
|--------|--------|---------|
| Proposal | Seed → Planned → Approved → Implementing → Completed → Verified (+ Scratched) | Engineering proposals |
| CcSession | Idle → Running → Completed / Failed / TimedOut | Claude Code sessions |
| Deployment | Pending → Staging → Production / RolledBack | Deployment tracking |
| Finding | Observed → Triaged → Resolved / WontFix | Bug/issue findings |

## Multi-Tenant

Other agents (Calcifer, Jiji, Chihiro) can run their own apps on the same Temper instance:

```bash
temper serve \
  --app haku-ops=apps/haku-ops/specs \
  --app calcifer-ops=apps/calcifer-ops/specs \
  --port 3001
```

Each app gets its own tenant, entity types, and state machines. Data is fully isolated by tenant in Postgres.
